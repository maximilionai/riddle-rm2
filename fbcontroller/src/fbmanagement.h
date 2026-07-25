// qtfb server -- SOCK_SEQPACKET control socket + POSIX shared memory.
//
// Ported from asivery/rm-appload src/qtfb/fbmanagement.h.
// Changes vs upstream:
//   * the two global maps are declared `extern` here and defined once in the
//     .cpp (upstream declares them `static` in the header, giving every
//     translation unit its own copy).
//   * the maps are guarded by one recursive mutex on every path, including
//     the ones upstream leaves unguarded (registerController /
//     unregisterController / isControllerAssociated all run on the GUI thread
//     while worker threads mutate the same maps).
#pragma once

#include <map>
#include <vector>

#include <QtCore/QPointer>
#include <QtGui/QImage>

#include "FBController.h"
#include "qtfb_common.h"

#define SOCKET_BACKLOG 10
#define RESP_ERR 1
#define RESP_OK 0

namespace qtfb::management {

class ClientConnection;

class ClientBackend {
public:
    int shmFD = -1;
    int shmKey = -1;
    unsigned char *shm = nullptr;
    QImage *image = nullptr;
    int shmType = -1;
    size_t shmSize = 0;

    std::vector<ClientConnection *> connections;

    ~ClientBackend();
};

class ClientConnection {
public:
    int clientFD = -1;
    FBKey fbKey = -1;
};

extern std::map<FBKey, QPointer<FBController>> framebuffers;
extern std::map<FBKey, ClientBackend *> connections;

void registerController(FBKey key, QPointer<FBController> controller);
void unregisterController(FBKey key);
bool isControllerAssociated(FBKey key);

// Nominate a controller as the "wildcard": when a client initialises a key
// that no controller has claimed, the wildcard is re-keyed onto it.
//
// Upstream does not need this because on the Paper Pro rm-appload launches the
// client and therefore knows its key in advance. There is no AppLoad on this
// device -- riddle is started by hand with an arbitrary QTFB_KEY -- so the key
// has to flow client -> server instead of server -> client.
void setWildcardController(QPointer<FBController> controller);

void forwardUserInput(FBKey key, struct UserInputContents *input);

// Spawns the accept loop on a detached thread. Safe to call more than once;
// only the first call does anything. Must be called after QCoreApplication
// exists (it queues work onto the GUI thread).
void start();

} // namespace qtfb::management
