#include "screenmode.h"

#include <stdlib.h>

#include <QtCore/QByteArray>
#include <QtCore/QMetaEnum>
#include <QtCore/QMetaMethod>
#include <QtCore/QMetaObject>
#include <QtCore/QMetaProperty>
#include <QtCore/QObject>
#include <QtCore/QPointer>
#include <QtCore/QString>
#include <QtCore/QUrl>
#include <QtCore/QVariant>
#include <QtQml/QQmlComponent>
#include <QtQml/QQmlContext>
#include <QtQml/QQmlEngine>
#include <QtQml/QQmlError>
#include <QtQuick/QQuickItem>

#include <iostream>

#define SMERR std::cerr << "[QTFB/sm]: "

namespace {

const char *DEFAULT_URI = "com.remarkable";
const char *SM_OBJECT_NAME = "riddleFbScreenMode";

QByteArray buildQml(const char *uri, const char *element)
{
    QByteArray q;
    q += "import QtQuick 2.15\n";
    q += "import ";
    q += uri;
    q += " 1.0\n";
    q += element;
    q += "\n";
    return q;
}

void dumpMetaObject(const QMetaObject *mo, const char *what)
{
    if (!mo) {
        SMERR << what << ": <null metaobject>" << std::endl;
        return;
    }

    SMERR << "==== " << what << " -> class " << mo->className() << " ====" << std::endl;

    {
        std::string chain;
        for (const QMetaObject *s = mo->superClass(); s; s = s->superClass()) {
            chain += " <- ";
            chain += s->className();
        }
        SMERR << "  superclass chain:" << chain << std::endl;
    }

    SMERR << "  methods (" << mo->methodOffset() << ".." << (mo->methodCount() - 1)
          << "; own from " << mo->methodOffset() << "):" << std::endl;
    for (int i = mo->methodOffset(); i < mo->methodCount(); ++i) {
        const QMetaMethod m = mo->method(i);
        const char *kind = "method";
        switch (m.methodType()) {
        case QMetaMethod::Signal:      kind = "signal"; break;
        case QMetaMethod::Slot:        kind = "slot"; break;
        case QMetaMethod::Constructor: kind = "ctor"; break;
        default: break;
        }
        std::string params;
        const QList<QByteArray> ptypes = m.parameterTypes();
        const QList<QByteArray> pnames = m.parameterNames();
        for (int p = 0; p < ptypes.size(); ++p) {
            if (p) params += ", ";
            params += ptypes.at(p).constData();
            if (p < pnames.size() && !pnames.at(p).isEmpty()) {
                params += " ";
                params += pnames.at(p).constData();
            }
        }
        SMERR << "    [" << kind << "] "
              << (m.typeName() && *m.typeName() ? m.typeName() : "void") << " "
              << m.name().constData() << "(" << params << ")" << std::endl;
    }

    SMERR << "  properties:" << std::endl;
    for (int i = mo->propertyOffset(); i < mo->propertyCount(); ++i) {
        const QMetaProperty p = mo->property(i);
        SMERR << "    " << p.name() << " : " << p.typeName()
              << (p.isWritable() ? " (rw" : " (ro")
              << (p.hasNotifySignal() ? ", notify" : "")
              << (p.isEnumType() ? ", enum" : "") << ")" << std::endl;
    }

    SMERR << "  enums:" << std::endl;
    for (int i = mo->enumeratorOffset(); i < mo->enumeratorCount(); ++i) {
        const QMetaEnum e = mo->enumerator(i);
        std::string keys;
        for (int k = 0; k < e.keyCount(); ++k) {
            if (k) keys += ", ";
            keys += e.key(k);
            keys += "=";
            keys += std::to_string(e.value(k));
        }
        SMERR << "    " << e.name() << " { " << keys << " }" << std::endl;
    }
    SMERR << "==== end " << what << " ====" << std::endl;
}

// Compile+instantiate a throwaway QML snippet and hand the object to `fn`.
// The object is deleted before returning. Errors are logged, never fatal.
template <typename F>
void withTemporary(QQmlEngine *engine, QQmlContext *ctx, const QByteArray &qml,
                   const char *label, F fn)
{
    QQmlComponent comp(engine);
    comp.setData(qml, QUrl(QStringLiteral("qrc:/riddlefb/probe.qml")));
    if (comp.isError()) {
        for (const QQmlError &e : comp.errors())
            SMERR << label << ": component error: " << qPrintable(e.toString()) << std::endl;
        return;
    }
    QObject *o = comp.create(ctx);
    if (!o) {
        SMERR << label << ": create() returned null" << std::endl;
        for (const QQmlError &e : comp.errors())
            SMERR << label << ": " << qPrintable(e.toString()) << std::endl;
        return;
    }
    QQmlEngine::setObjectOwnership(o, QQmlEngine::CppOwnership);
    fn(o);
    delete o;
}

} // namespace

namespace qtfb {
namespace screenmode {

void dumpProbe(QQuickItem *anchor)
{
    if (!anchor) {
        SMERR << "probe: no anchor item" << std::endl;
        return;
    }

    QQmlEngine *engine = qmlEngine(anchor);
    QQmlContext *ctx = qmlContext(anchor);
    SMERR << "probe: anchor=" << anchor->metaObject()->className()
          << " engine=" << (void *) engine << " context=" << (void *) ctx << std::endl;

    if (!engine) {
        // The FBController is created from C++ and parented onto a QML item, so
        // qmlEngine() on the anchor can be null. Walk up to a QML-owned parent.
        for (QQuickItem *p = anchor->parentItem(); p; p = p->parentItem()) {
            if ((engine = qmlEngine(p))) {
                if (!ctx) ctx = qmlContext(p);
                SMERR << "probe: found engine via parent "
                      << p->metaObject()->className() << std::endl;
                break;
            }
        }
    }
    if (!engine) {
        SMERR << "probe: no QQmlEngine reachable; giving up (nothing was called)"
              << std::endl;
        return;
    }
    if (!ctx)
        ctx = engine->rootContext();

    // ---- route 1: root/attached context property ------------------------
    for (QQmlContext *c = ctx; c; c = c->parentContext()) {
        const QVariant v = c->contextProperty(QStringLiteral("EPFramebuffer"));
        if (v.isValid()) {
            SMERR << "probe: EPFramebuffer found as a context property on "
                  << (void *) c << std::endl;
            if (QObject *o = v.value<QObject *>())
                dumpMetaObject(o->metaObject(), "EPFramebuffer (context property)");
            break;
        }
    }

    // ---- route 2: QML singleton -----------------------------------------
#if QT_VERSION >= QT_VERSION_CHECK(6, 5, 0)
    {
        QObject *fb = engine->singletonInstance<QObject *>(QStringLiteral("com.remarkable"),
                                                           QStringLiteral("EPFramebuffer"));
        if (fb)
            dumpMetaObject(fb->metaObject(), "EPFramebuffer (QQmlEngine::singletonInstance)");
        else
            SMERR << "probe: singletonInstance(com.remarkable, EPFramebuffer) = null"
                  << std::endl;
    }
#endif

    // ---- route 3: resolve it through a throwaway QML object -------------
    withTemporary(engine, ctx,
                  buildQml(DEFAULT_URI,
                           "QtObject { property var fb: EPFramebuffer }"),
                  "EPFramebuffer-via-QML", [](QObject *o) {
                      QObject *fb = o->property("fb").value<QObject *>();
                      if (fb)
                          dumpMetaObject(fb->metaObject(), "EPFramebuffer (QML binding)");
                      else
                          SMERR << "probe: QML binding yielded no EPFramebuffer" << std::endl;
                  });

    // ---- route 4: the type that actually matters ------------------------
    withTemporary(engine, ctx, buildQml(DEFAULT_URI, "EPScreenMode { }"),
                  "EPScreenMode", [](QObject *o) {
                      dumpMetaObject(o->metaObject(), "EPScreenMode (fresh instance)");
                      SMERR << "probe: EPScreenMode is a QQuickItem: "
                            << (qobject_cast<QQuickItem *>(o) ? "yes" : "NO") << std::endl;
                  });
}

QQuickItem *attach(QQuickItem *host, int mode, const char *uri)
{
    if (!host) {
        SMERR << "attach: no host item" << std::endl;
        return nullptr;
    }
    if (!uri || !*uri)
        uri = DEFAULT_URI;

    QQmlEngine *engine = qmlEngine(host);
    QQmlContext *ctx = qmlContext(host);
    for (QQuickItem *p = host->parentItem(); p && !engine; p = p->parentItem()) {
        engine = qmlEngine(p);
        if (!ctx) ctx = qmlContext(p);
    }
    if (!engine) {
        SMERR << "attach: no QQmlEngine reachable from the host item; "
                 "cannot create EPScreenMode" << std::endl;
        return nullptr;
    }
    if (!ctx)
        ctx = engine->rootContext();

    QQmlComponent comp(engine);
    comp.setData(buildQml(uri, "EPScreenMode { }"),
                 QUrl(QStringLiteral("qrc:/riddlefb/screenmode.qml")));
    if (comp.isError()) {
        for (const QQmlError &e : comp.errors())
            SMERR << "attach: " << qPrintable(e.toString()) << std::endl;
        SMERR << "attach: EPScreenMode unavailable from module '" << uri
              << "'; continuing without a waveform hint" << std::endl;
        return nullptr;
    }

    QObject *o = comp.create(ctx);
    if (!o) {
        SMERR << "attach: EPScreenMode create() failed" << std::endl;
        return nullptr;
    }
    QQmlEngine::setObjectOwnership(o, QQmlEngine::CppOwnership);

    QQuickItem *sm = qobject_cast<QQuickItem *>(o);
    if (!sm) {
        SMERR << "attach: EPScreenMode is not a QQuickItem (" << o->metaObject()->className()
              << "); discarding" << std::endl;
        delete o;
        return nullptr;
    }

    const int modeIdx = sm->metaObject()->indexOfProperty("mode");
    if (modeIdx < 0) {
        SMERR << "attach: EPScreenMode has no 'mode' property; discarding" << std::endl;
        delete sm;
        return nullptr;
    }
    if (!sm->setProperty("mode", mode))
        SMERR << "attach: setProperty(mode, " << mode << ") was refused" << std::endl;

    sm->setObjectName(QString::fromLatin1(SM_OBJECT_NAME));
    sm->setParent(host);
    sm->setParentItem(host);

    // The node it emits is (0,0,width,height) in item coordinates, so it must
    // have a real size. Mirror the host exactly.
    auto sync = [sm, host]() {
        sm->setX(0);
        sm->setY(0);
        sm->setWidth(host->width());
        sm->setHeight(host->height());
    };
    sync();
    QObject::connect(host, &QQuickItem::widthChanged, sm, sync);
    QObject::connect(host, &QQuickItem::heightChanged, sm, sync);

    // Above the host's own content, like xochitl's own usage (last sibling).
    sm->setZ(1);
    sm->setVisible(true);

    SMERR << "attach: EPScreenMode attached to " << host->metaObject()->className()
          << " with mode=" << sm->property("mode").toInt()
          << " size=" << (int) sm->width() << "x" << (int) sm->height() << std::endl;
    return sm;
}

} // namespace screenmode
} // namespace qtfb
