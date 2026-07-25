// xovi extension entry point for the rM2 qtfb server.
//
// Two independent things happen here:
//
//  1. _xovi_construct() (runs pre-main, from xovi.so's ELF constructor)
//     registers FBController as a QML type. This only writes to Qt's static
//     type registry -- it does not need QCoreApplication and does not touch
//     the filesystem or the network, so it cannot stall xochitl's startup.
//     It also arms a Qt "pre routine" via qAddPreRoutine().
//
//  2. The pre routine runs immediately after QCoreApplication is constructed,
//     on the GUI thread. It starts the qtfb server thread and arms a short
//     poll timer that attaches an FBController to xochitl's QQuickWindow as
//     soon as one exists.
//
// The attach step deliberately does NOT depend on any QML change: if no
// injected slot item is found it parents straight onto the window's
// contentItem. That means the .so is useful with or without a .qmd, and a
// missing/broken .qmd can never leave xochitl without a UI.
#include <stdlib.h>
#include <string.h>

#include <QtCore/QCoreApplication>
#include <QtCore/QMetaObject>
#include <QtCore/QPointer>
#include <QtCore/QTimer>
#include <QtGui/QGuiApplication>
#include <QtGui/QWindow>
#include <QtQml/QQmlEngine>
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

#include "FBController.h"
#include "fbmanagement.h"
#include "screenmode.h"
#include "qtfb_log.h"

// QML module URI the type is registered under. Only needed if you use the
// "type" flavour of the .qmd; the default slot/contentItem flavour never
// imports it.
#define RIDDLEFB_URI "riddlefb"
#define RIDDLEFB_VERSION_MAJOR 1
#define RIDDLEFB_VERSION_MINOR 0

// objectName of an optional QML placeholder to parent onto.
static const char *SLOT_OBJECT_NAME = "riddleFbSlot";

static QPointer<FBController> g_controller;
static QTimer *g_attachTimer = nullptr;

static int envInt(const char *name, int fallback)
{
    const char *v = getenv(name);
    if (!v || !*v)
        return fallback;
    char *end = nullptr;
    const long r = strtol(v, &end, 0);
    if (end == v)
        return fallback;
    return (int) r;
}

static bool envFlag(const char *name, bool fallback)
{
    const char *v = getenv(name);
    if (!v || !*v)
        return fallback;
    return !(v[0] == '0' || v[0] == 'n' || v[0] == 'N' || v[0] == 'f' || v[0] == 'F');
}

static void bindGeometry(FBController *c, QQuickItem *host)
{
    auto apply = [c, host]() {
        const qreal w = host->width() > 0 ? host->width() : (qreal) RM2_WIDTH;
        const qreal h = host->height() > 0 ? host->height() : (qreal) RM2_HEIGHT;
        c->setX(0);
        c->setY(0);
        c->setWidth(w);
        c->setHeight(h);
    };
    apply();
    QObject::connect(host, &QQuickItem::widthChanged, c, apply);
    QObject::connect(host, &QQuickItem::heightChanged, c, apply);
}

// Returns true once a controller exists (either adopted or created).
static bool tryAttach()
{
    if (!g_controller.isNull())
        return true;

    const QWindowList windows = QGuiApplication::topLevelWindows();
    for (QWindow *w : windows) {
        auto *qw = qobject_cast<QQuickWindow *>(w);
        if (!qw || !qw->contentItem())
            continue;

        // If a .qmd already instantiated an FBController, adopt it rather than
        // making a second one (two controllers on one key would be refused).
        if (auto *existing = qw->findChild<FBController *>()) {
            g_controller = existing;
            CERR << "Adopted an FBController that QML already created." << std::endl;
            return true;
        }

        QQuickItem *host = nullptr;
        if (QObject *slot = qw->findChild<QObject *>(QString::fromLatin1(SLOT_OBJECT_NAME)))
            host = qobject_cast<QQuickItem *>(slot);
        const bool usedSlot = (host != nullptr);
        if (!host)
            host = qw->contentItem();

        auto *c = new FBController(host);
        c->setObjectName(QStringLiteral("riddleFbController"));
        c->setZ((qreal) envInt("RIDDLE_FB_Z", 10000));
        c->setAutoHide(envFlag("RIDDLE_FB_AUTOHIDE", true));
        bindGeometry(c, host);
        c->setFramebufferID(envInt("RIDDLE_FB_KEY", QTFB_DEFAULT_FRAMEBUFFER));

        // There is no AppLoad on this device to agree a key with the client, so
        // the client picks one (riddle: QTFB_KEY=<n>) and we adopt it.
        if (envFlag("RIDDLE_FB_ADOPT_KEY", true))
            qtfb::management::setWildcardController(c);

        // Fast e-ink waveform. xochitl picks the panel waveform per region from
        // EPScreenMode items in the scene graph; with none covering our damage,
        // EPFramebufferSwtcon::update() falls through to the slow Gc16Fast
        // grayscale waveform -- which is why ink lagged and caught up in steps.
        // Parented onto the FBController, so it hides with the overlay and
        // leaves xochitl's own waveform selection untouched when idle.
        if (envFlag("RIDDLE_FB_PROBE", false))
            qtfb::screenmode::dumpProbe(c);
        {
            const int sm = envInt("RIDDLE_FB_SCREENMODE", qtfb::screenmode::ModeMono);
            if (sm >= 0)
                qtfb::screenmode::attach(c, sm, getenv("RIDDLE_FB_SCREENMODE_URI"));
        }

        g_controller = c;
        CERR << "Attached FBController to "
             << (usedSlot ? "the injected QML slot 'riddleFbSlot'"
                          : "the window contentItem (no slot found)")
             << "; key " << c->framebufferID() << ", size " << (int) c->width() << "x"
             << (int) c->height() << std::endl;
        return true;
    }
    return false;
}

// Runs on the GUI thread right after QCoreApplication is constructed.
extern "C" void riddlefb_startup()
{
    if (!envFlag("RIDDLE_FB_ENABLE", true)) {
        CERR << "RIDDLE_FB_ENABLE is off; not starting the qtfb server." << std::endl;
        return;
    }

    qtfb::management::start();

    if (!envFlag("RIDDLE_FB_AUTOATTACH", true)) {
        CERR << "RIDDLE_FB_AUTOATTACH is off; expecting QML to create the item." << std::endl;
        return;
    }

    QCoreApplication *app = QCoreApplication::instance();
    if (!app)
        return;

    const int fastPoll = envInt("RIDDLE_FB_POLL_MS", 250);
    g_attachTimer = new QTimer(app);
    g_attachTimer->setInterval(fastPoll);
    // The timer is never stopped: once attached the callback is a single
    // QPointer null-check, and keeping it alive means we re-attach by ourselves
    // if xochitl ever tears its window down and builds a new one.
    QObject::connect(g_attachTimer, &QTimer::timeout, app, [fastPoll]() {
        const bool wasAttached = !g_controller.isNull();
        if (tryAttach()) {
            if (!wasAttached && g_attachTimer)
                g_attachTimer->setInterval(2000); // idle watch
        } else if (g_attachTimer && g_attachTimer->interval() != fastPoll) {
            g_attachTimer->setInterval(fastPoll); // window vanished, hunt again
        }
    });
    g_attachTimer->start();
}

extern "C" void _xovi_construct(void)
{
    // Safe before QCoreApplication exists: this only mutates Qt's static QML
    // type registry.
    qmlRegisterType<FBController>(RIDDLEFB_URI, RIDDLEFB_VERSION_MAJOR,
                                  RIDDLEFB_VERSION_MINOR, "FBController");
    qAddPreRoutine(riddlefb_startup);
    CERR << "riddlefb extension constructed; QML type " << RIDDLEFB_URI << " "
         << RIDDLEFB_VERSION_MAJOR << "." << RIDDLEFB_VERSION_MINOR
         << " FBController registered." << std::endl;
}
