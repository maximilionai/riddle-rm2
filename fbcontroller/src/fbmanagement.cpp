// qtfb server, ported from asivery/rm-appload src/qtfb/fbmanagement.cpp.
//
// Protocol (unchanged from upstream, see qtfb_common.h for the pinned layout):
//   * clients connect to a SOCK_SEQPACKET UNIX socket at /tmp/qtfb.sock
//   * client sends MESSAGE_INITIALIZE{key, format}
//   * server allocates /dev/shm/qtfb_<shmKey> and replies with {shmKey, shmSize}
//   * client writes pixels into the shm, then sends MESSAGE_UPDATE
//   * server queues a repaint of the matching FBController onto the GUI thread
//
// rM2-specific changes vs upstream:
//   * every default-resolution request resolves to 1404x1872 (the only screen
//     this device has); the format byte selects the *pixel layout* only
//   * MSG_NOSIGNAL on every send()  -- upstream can take SIGPIPE and kill the
//     host process when a client dies mid-write
//   * backend teardown is queued onto the GUI thread instead of spinning on
//     FBController::isMidPaint, so no worker thread can stall (WatchdogSec=60)
//   * no srand()/rand(): we do not perturb the host application's RNG
//   * sockaddr_un is zeroed before use
#include "fbmanagement.h"
#include "qtfb_log.h"

#include <algorithm>
#include <functional>
#include <mutex>
#include <random>
#include <thread>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#include <QtCore/QCoreApplication>
#include <QtCore/QMetaObject>
#include <QtCore/QRect>

namespace qtfb::management {
std::map<FBKey, QPointer<FBController>> framebuffers;
std::map<FBKey, ClientBackend *> connections;
} // namespace qtfb::management

static std::recursive_mutex globalBackendsListMutex;
#define SYNCHRONIZE const std::lock_guard<std::recursive_mutex> __lock(globalBackendsListMutex)

// Never block: not every client is actively reading. Drop rather than stall,
// and never let a dead peer raise SIGPIPE in the host process.
#define SEND(fd, message) \
    ::send((fd), &(message), sizeof(message), MSG_DONTWAIT | MSG_NOSIGNAL)

// ---------------------------------------------------------------------------
// Format handling
// ---------------------------------------------------------------------------

struct FormatInfo {
    QImage::Format qformat;
    int bytesPerPixel;
    const char *name;
};

// Canonical rM2 format is FBFMT_RMPP_RGB565 (3) reinterpreted with rM2
// geometry: 1404x1872 RGB565 LE, stride 2808. FBFMT_RM2FB (0) and
// FBFMT_RMPPM_RGB565 (6) are exact aliases of it.
static bool describeFormat(int shmType, FormatInfo *out)
{
    switch (shmType) {
    case FBFMT_RMPP_RGB565:
        *out = { QImage::Format_RGB16, 2, "RGB565" };
        return true;
    case FBFMT_RM2FB:
    case FBFMT_RMPPM_RGB565:
        *out = { QImage::Format_RGB16, 2, "RGB565 (alias of FBFMT_RMPP_RGB565)" };
        return true;
    case FBFMT_RMPP_RGB888:
    case FBFMT_RMPPM_RGB888:
        *out = { QImage::Format_RGB888, 3, "RGB888" };
        return true;
    case FBFMT_RMPP_RGBA8888:
    case FBFMT_RMPPM_RGBA8888:
        *out = { QImage::Format_RGBA8888, 4, "RGBA8888" };
        return true;
    default:
        return false;
    }
}

// Collapse the Paper Pro aliases onto the canonical code so that a second
// client attaching to the same key with code 3 instead of 0 is not rejected.
static int normalizeFormat(int shmType)
{
    switch (shmType) {
    case FBFMT_RM2FB:
    case FBFMT_RMPPM_RGB565:
        return FBFMT_RMPP_RGB565;
    case FBFMT_RMPPM_RGB888:
        return FBFMT_RMPP_RGB888;
    case FBFMT_RMPPM_RGBA8888:
        return FBFMT_RMPP_RGBA8888;
    default:
        return shmType;
    }
}

static int randomShmKey()
{
    static std::mt19937 rng(std::random_device {}());
    static std::mutex rngMutex;
    const std::lock_guard<std::mutex> lock(rngMutex);
    return (int) (rng() & 0x7FFFFFFFu);
}

// ---------------------------------------------------------------------------
// Backend lifetime
// ---------------------------------------------------------------------------

// The controller that adopts an unclaimed key. Only ever touched with
// globalBackendsListMutex held.
static QPointer<FBController> g_wildcard;

void qtfb::management::setWildcardController(QPointer<FBController> controller)
{
    SYNCHRONIZE;
    g_wildcard = controller;
}

// Caller must hold the lock. True if the wildcard exists and is not currently
// serving a live client under some other key.
static bool wildcardIsAvailable()
{
    using namespace qtfb::management;
    if (g_wildcard.isNull())
        return false;
    for (const auto &kv : framebuffers) {
        if (kv.second == g_wildcard)
            return connections.find(kv.first) == connections.end();
    }
    return true; // registered nowhere, so free
}

static void tryToMatchUp(qtfb::FBKey key)
{
    using namespace qtfb::management;
    auto conn = connections.find(key);
    if (conn == connections.end())
        return; // client not here yet
    auto fb = framebuffers.find(key);
    if (fb == framebuffers.end()) {
        // No controller has claimed this key. Hand it to the wildcard, which is
        // how a hand-launched client gets to choose its own key. The re-key is
        // queued onto the GUI thread; FBController::setFramebufferID() calls
        // registerController(), which lands back here with both halves present.
        if (wildcardIsAvailable()) {
            QPointer<FBController> w = g_wildcard;
            CERR << "No controller for key " << key << "; re-keying the wildcard onto it."
                 << std::endl;
            QMetaObject::invokeMethod(w, [w, key]() {
                if (!w.isNull())
                    w->setFramebufferID(key);
            }, Qt::QueuedConnection);
        }
        return; // controller not here yet
    }

    ClientBackend *connection = conn->second;
    QPointer<FBController> controller = fb->second;
    if (connection->shm == nullptr || controller.isNull()) {
        CERR << "Invalid state: partially-associated connection in the map!" << std::endl;
        return;
    }
    controller->associateSHM(connection->image);
    CERR << "Associated connection <==> framebuffer " << key << std::endl;
}

static bool createSHM(qtfb::management::ClientBackend *connection, int shmType, int width, int height)
{
    FormatInfo info;
    if (!describeFormat(shmType, &info)) {
        CERR << "Unknown SHM type " << shmType << std::endl;
        return false;
    }
    if (width <= 0 || height <= 0 || width > 8192 || height > 8192) {
        CERR << "Refusing absurd framebuffer geometry " << width << "x" << height << std::endl;
        return false;
    }

    const int bpl = info.bytesPerPixel * width;
    const size_t shmSize = (size_t) bpl * (size_t) height;

    CERR << "Client connecting in mode " << shmType << " (" << info.name << "), resolution "
         << width << "x" << height << ", stride " << bpl << std::endl;

    connection->shmSize = shmSize;
    connection->shmType = shmType;
    connection->shmKey = randomShmKey();
    FORMAT_SHM(shmText, connection->shmKey);
    shm_unlink(shmText);
    connection->shmFD = shm_open(shmText, O_RDWR | O_CREAT | O_EXCL,
                                 S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP | S_IROTH | S_IWOTH);
    if (connection->shmFD == -1) {
        CERR << "Failed to shm_open() " << shmText << ": " << strerror(errno) << std::endl;
        connection->shmKey = -1;
        connection->shmSize = 0;
        connection->shmType = -1;
        return false;
    }
    // shm_open() honours umask; the client may not be root.
    fchmod(connection->shmFD, 0666);
    if (ftruncate(connection->shmFD, (off_t) shmSize) == -1) {
        CERR << "Failed to ftruncate() the SHM: " << strerror(errno) << std::endl;
        close(connection->shmFD);
        shm_unlink(shmText);
        connection->shmFD = -1;
        connection->shmKey = -1;
        connection->shmSize = 0;
        connection->shmType = -1;
        return false;
    }
    connection->shm = (unsigned char *) mmap(nullptr, shmSize, PROT_READ | PROT_WRITE,
                                             MAP_SHARED, connection->shmFD, 0);
    if (connection->shm == MAP_FAILED) {
        CERR << "Failed to mmap() the SHM for the framebuffer: " << strerror(errno) << std::endl;
        close(connection->shmFD);
        shm_unlink(shmText);
        connection->shmFD = -1;
        connection->shm = nullptr;
        connection->shmSize = 0;
        connection->shmType = -1;
        connection->shmKey = -1;
        return false;
    }
    // Start white; on e-ink an unset buffer flashing black is very visible.
    memset(connection->shm, 0xFF, shmSize);
    CERR << "Defined SHM (" << shmSize << " bytes) as " << shmText << std::endl;

    connection->image = new QImage(connection->shm, width, height, bpl, info.qformat,
                                   nullptr, nullptr);
    return true;
}

// On the rM2 there is exactly one panel geometry, so every default-resolution
// request resolves to it regardless of which device's format code was sent.
static bool createDefaultSHM(qtfb::management::ClientBackend *connection, int shmType)
{
    return createSHM(connection, shmType, RM2_WIDTH, RM2_HEIGHT);
}

qtfb::management::ClientBackend::~ClientBackend()
{
    delete image;
    if (shm != nullptr)
        munmap(shm, shmSize);
    if (shmFD != -1)
        close(shmFD);
    if (shmKey != -1) {
        FORMAT_SHM(shmName, shmKey);
        shm_unlink(shmName);
    }
}

// ---------------------------------------------------------------------------
// Controller registry (GUI thread side)
// ---------------------------------------------------------------------------

void qtfb::management::registerController(FBKey key, QPointer<FBController> controller)
{
    if (key == -1)
        return;
    SYNCHRONIZE;
    auto existing = framebuffers.find(key);
    if (existing != framebuffers.end()) {
        if (!existing->second.isNull() && existing->second != controller) {
            CERR << "Violation: framebuffer " << key
                 << " is already claimed. Change the framebufferID!" << std::endl;
            return;
        }
        framebuffers.erase(existing); // stale QPointer, reclaim it
    }
    CERR << "Registered framebuffer controller ID: " << key << std::endl;
    framebuffers.emplace(key, controller);
    tryToMatchUp(key);
}

bool qtfb::management::isControllerAssociated(FBKey key)
{
    SYNCHRONIZE;
    return connections.find(key) != connections.end();
}

void qtfb::management::unregisterController(FBKey key)
{
    if (key == -1)
        return;
    SYNCHRONIZE;
    auto position = framebuffers.find(key);
    if (position != framebuffers.end())
        framebuffers.erase(position);
    CERR << "Unregistered framebuffer controller ID: " << key << std::endl;
}

void qtfb::management::forwardUserInput(FBKey key, struct UserInputContents *input)
{
    SYNCHRONIZE;
    auto position = connections.find(key);
    if (position == connections.end())
        return;
    ClientBackend *backend = position->second;
    ServerMessage outbound {};
    outbound.type = MESSAGE_USERINPUT;
    outbound.userInput = *input;
    for (ClientConnection *connection : backend->connections)
        SEND(connection->clientFD, outbound);
}

// ---------------------------------------------------------------------------
// Client worker thread
// ---------------------------------------------------------------------------

static int handleInitialize(qtfb::management::ClientConnection *connection,
                            qtfb::ClientMessage *inbound, int messageType)
{
    using namespace qtfb::management;
    SYNCHRONIZE;

    const qtfb::FBKey key = inbound->init.framebufferKey;
    const int requested = normalizeFormat(messageType == MESSAGE_CUSTOM_INITIALIZE
                                              ? inbound->customInit.framebufferType
                                              : inbound->init.framebufferType);
    connection->fbKey = key;

    auto existing = connections.find(key);
    if (existing != connections.end()) {
        // A backend already exists. Only safe if the parameters agree.
        ClientBackend *backend = existing->second;
        if (backend->image == nullptr)
            return RESP_ERR;
        switch (messageType) {
        case MESSAGE_CUSTOM_INITIALIZE:
            if (backend->shmType != requested
                || backend->image->width() != inbound->customInit.width
                || backend->image->height() != inbound->customInit.height)
                return RESP_ERR;
            break;
        case MESSAGE_INITIALIZE:
            if (backend->shmType != requested)
                return RESP_ERR;
            break;
        default:
            return RESP_ERR;
        }
        qtfb::ServerMessage outbound {};
        outbound.type = MESSAGE_INITIALIZE;
        outbound.init.shmKeyDefined = backend->shmKey;
        outbound.init.shmSize = (uint64_t) backend->shmSize;
        SEND(connection->clientFD, outbound);
        backend->connections.push_back(connection);
        return RESP_OK;
    }

    ClientBackend *newBackend = new ClientBackend();
    bool result = false;
    switch (messageType) {
    case MESSAGE_CUSTOM_INITIALIZE:
        result = createSHM(newBackend, requested, inbound->customInit.width,
                           inbound->customInit.height);
        break;
    case MESSAGE_INITIALIZE:
        result = createDefaultSHM(newBackend, requested);
        break;
    default:
        break;
    }
    if (!result) {
        delete newBackend;
        return RESP_ERR;
    }

    qtfb::ServerMessage outbound {};
    outbound.type = MESSAGE_INITIALIZE;
    outbound.init.shmKeyDefined = newBackend->shmKey;
    outbound.init.shmSize = (uint64_t) newBackend->shmSize;
    SEND(connection->clientFD, outbound);

    newBackend->connections.push_back(connection);
    connections[key] = newBackend;
    tryToMatchUp(key);
    return RESP_OK;
}

static int handleUpdateRegion(QPointer<FBController> controller,
                              qtfb::management::ClientConnection *connection,
                              qtfb::ClientMessage *inbound)
{
    switch (inbound->update.type) {
    case UPDATE_ALL:
        QMetaObject::invokeMethod(controller, [controller]() {
            if (!controller.isNull())
                controller->markedUpdate();
        }, Qt::QueuedConnection);
        break;
    case UPDATE_PARTIAL: {
        const int x = inbound->update.x, y = inbound->update.y;
        const int w = inbound->update.w, h = inbound->update.h;
        QMetaObject::invokeMethod(controller, [controller, x, y, w, h]() {
            if (!controller.isNull())
                controller->markedUpdate(QRect(x, y, w, h));
        }, Qt::QueuedConnection);
        break;
    }
    default:
        CERR << "Unknown update method " << inbound->update.type << " for framebuffer "
             << connection->fbKey << std::endl;
        break;
    }
    return RESP_OK;
}

static int invokeOnConnectedFramebuffer(qtfb::management::ClientConnection *connection,
                                        std::function<int(QPointer<FBController>)> consumer)
{
    using namespace qtfb::management;
    if (connection->fbKey == -1) {
        CERR << "Cannot act on an uninitialised connection!" << std::endl;
        return RESP_ERR;
    }
    QPointer<FBController> controller;
    {
        SYNCHRONIZE;
        auto it = framebuffers.find(connection->fbKey);
        if (it == framebuffers.end()) {
            CERR << "Could not find the framebuffer to act upon." << std::endl;
            return RESP_OK;
        }
        controller = it->second;
    }
    if (controller.isNull())
        return RESP_OK;
    return consumer(controller);
}

static void managementClientThread(int incomingFD)
{
    using namespace qtfb::management;
    ClientConnection connection;
    connection.clientFD = incomingFD;

    CERR << "Connection established, fd " << incomingFD << std::endl;

    qtfb::ClientMessage inboundMessage;
    for (;;) {
        const ssize_t got = recv(incomingFD, &inboundMessage, sizeof(inboundMessage), 0);
        if (got < 0 && errno == EINTR)
            continue;
        if (got < 1)
            break;
        int status = RESP_OK;
        switch (inboundMessage.type) {
        case MESSAGE_INITIALIZE:
        case MESSAGE_CUSTOM_INITIALIZE:
            status = handleInitialize(&connection, &inboundMessage, inboundMessage.type);
            break;
        case MESSAGE_UPDATE:
            status = invokeOnConnectedFramebuffer(&connection, [&](QPointer<FBController> c) {
                return handleUpdateRegion(c, &connection, &inboundMessage);
            });
            break;
        case MESSAGE_TERMINATE:
            CERR << "Client requested closing the connection." << std::endl;
            goto close_connection;
        case MESSAGE_SET_REFRESH_MODE: {
            const int refresh = inboundMessage.refreshMode;
            status = invokeOnConnectedFramebuffer(&connection, [refresh](QPointer<FBController> c) {
                if (refresh > 4 || refresh < 0) {
                    CERR << "Client tried to set an undefined refresh mode!" << std::endl;
                    return RESP_ERR;
                }
                QMetaObject::invokeMethod(c, [c, refresh]() {
                    if (!c.isNull())
                        c->setRefreshMode(refresh);
                }, Qt::QueuedConnection);
                return RESP_OK;
            });
            break;
        }
        case MESSAGE_REQUEST_FULL_REFRESH:
            status = invokeOnConnectedFramebuffer(&connection, [](QPointer<FBController> c) {
                QMetaObject::invokeMethod(c, [c]() {
                    if (!c.isNull())
                        emit c->requestFullRefresh();
                }, Qt::QueuedConnection);
                return RESP_OK;
            });
            break;
        default:
            CERR << "Client sent an invalid message type: " << (int) inboundMessage.type << std::endl;
            goto close_connection;
        }
        if (status == RESP_ERR) {
            CERR << "Request-specific error. Disconnecting..." << std::endl;
            goto close_connection;
        }
    }

close_connection:
    ClientBackend *backendToKill = nullptr;
    {
        SYNCHRONIZE;
        if (connection.fbKey != -1) {
            auto position = connections.find(connection.fbKey);
            if (position != connections.end()) {
                ClientBackend *backend = position->second;
                auto it = std::find(backend->connections.begin(), backend->connections.end(),
                                    &connection);
                if (it != backend->connections.end())
                    backend->connections.erase(it);
                if (backend->connections.empty()) {
                    backendToKill = backend;
                    connections.erase(position);
                }
            }
        }

        if (backendToKill != nullptr && connection.fbKey != -1) {
            auto fb = framebuffers.find(connection.fbKey);
            if (fb != framebuffers.end() && !fb->second.isNull()) {
                // Queues a setActive(false) onto the GUI thread; the delete
                // below is queued afterwards, so the controller has dropped
                // its QImage* before we munmap().
                fb->second->associateSHM(nullptr);
                CERR << "Disassociating framebuffer " << connection.fbKey << std::endl;
            }
        }
    }

    CERR << "Closing client socket " << incomingFD << std::endl;
    close(incomingFD);

    if (backendToKill != nullptr) {
        // Never munmap() from this thread: the GUI thread may still be holding
        // the QImage that points into the mapping. Ordering is guaranteed
        // because associateSHM(nullptr) was queued first.
        QCoreApplication *app = QCoreApplication::instance();
        if (app) {
            QMetaObject::invokeMethod(app, [backendToKill]() { delete backendToKill; },
                                      Qt::QueuedConnection);
        } else {
            delete backendToKill;
        }
    }
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

static void managementMainThread()
{
    CERR << "Starting qtfb server on " << SOCKET_PATH << std::endl;
    const int serverSocket = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    if (serverSocket == -1) {
        CERR << "Failed to create the socket: " << strerror(errno) << std::endl;
        return;
    }
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr)); // upstream leaves this uninitialised
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, SOCKET_PATH, sizeof(addr.sun_path) - 1);
    unlink(SOCKET_PATH);
    if (bind(serverSocket, (struct sockaddr *) &addr, sizeof(addr)) == -1) {
        CERR << "Failed to bind the socket: " << strerror(errno) << std::endl;
        close(serverSocket);
        return;
    }
    chmod(SOCKET_PATH, 0666);
    if (listen(serverSocket, SOCKET_BACKLOG) == -1) {
        CERR << "Failed to listen on the socket: " << strerror(errno) << std::endl;
        close(serverSocket);
        return;
    }
    CERR << "Awaiting incoming connections on " << SOCKET_PATH << std::endl;
    for (;;) {
        const int incomingFD = accept(serverSocket, nullptr, nullptr);
        if (incomingFD == -1) {
            if (errno == EINTR)
                continue;
            CERR << "accept() failed: " << strerror(errno) << std::endl;
            usleep(200 * 1000); // do not spin a core on a persistent error
            continue;
        }
        std::thread(managementClientThread, incomingFD).detach();
    }
}

void qtfb::management::start()
{
    static std::once_flag once;
    std::call_once(once, []() { std::thread(managementMainThread).detach(); });
}
