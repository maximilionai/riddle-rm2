// FBController -- QQuickPaintedItem that blits a client's shared-memory
// framebuffer into xochitl's Qt Quick scene graph.
//
// Ported from asivery/rm-appload src/qtfb/FBController.h for
// reMarkable 2 / armv7 / Qt 6.5.2 / 1404x1872.
#pragma once

#include <atomic>

#include <QtCore/QObject>
#include <QtCore/QPoint>
#include <QtCore/QPointF>
#include <QtCore/QRect>
#include <QtGui/QImage>
#include <QtGui/QPainter>
#include <QtQuick/QQuickPaintedItem>

#include "qtfb_common.h"

class FBController : public QQuickPaintedItem
{
    Q_OBJECT
    Q_PROPERTY(bool active READ active NOTIFY activeChanged)
    Q_PROPERTY(int framebufferID READ framebufferID WRITE setFramebufferID)
    Q_PROPERTY(bool allowScaling READ allowScaling WRITE setAllowScaling)
    Q_PROPERTY(int refreshMode READ refreshMode NOTIFY refreshModeChanged)
    // rM2 addition: hide the item completely while no client is attached, so a
    // dormant FBController cannot steal input or paint over xochitl's UI.
    Q_PROPERTY(bool autoHide READ autoHide WRITE setAutoHide)

public:
    explicit FBController(QQuickItem *parent = nullptr);
    ~FBController() override;

    void setFramebufferID(int fbID);
    int framebufferID() const;

    void setAllowScaling(bool a);
    bool allowScaling() const;

    void setAutoHide(bool a);
    bool autoHide() const;

    int refreshMode() const;
    void setRefreshMode(int refreshMode);

    bool active() const;

    void markedUpdate(const QRect &rect = QRect());
    void setActive(bool active); // NOT QML ACCESSIBLE!

    // Diagnostic: set while paint() is running. Read from server threads.
    std::atomic<bool> isMidPaint { false };

    void paint(QPainter *painter) override;
    void associateSHM(QImage *image);

    QPoint convertPointToQTFBPixels(const QPointF &input) const;

    void mousePressEvent(QMouseEvent *me) override;
    void mouseMoveEvent(QMouseEvent *me) override;
    void mouseReleaseEvent(QMouseEvent *me) override;
    void touchEvent(QTouchEvent *me) override;

    void keyPressEvent(QKeyEvent *ke) override;
    void keyReleaseEvent(QKeyEvent *ke) override;

    // rM2 addition: the rM2 digitiser can surface as QTabletEvent rather than
    // as synthesised mouse events depending on the Qt input plugin in use.
    bool event(QEvent *ev) override;

    Q_INVOKABLE void virtualKeyboardKeyDown(int key);
    Q_INVOKABLE void virtualKeyboardKeyUp(int key);

    Q_INVOKABLE void specialKeyDown(int key);
    Q_INVOKABLE void specialKeyUp(int key);

signals:
    void activeChanged();
    void dragDown();
    void requestFullRefresh();
    void refreshModeChanged();

private:
    void sendPointer(int type, const QPointF &pos, int devId, int pressure);

    int _framebufferID = -1;
    int _refreshMode = DEFAULT_WAVEFORM_MODE;
    bool _active = false;
    bool _allowScaling = false;
    bool _autoHide = true;

    bool checkingGestureDragDown = false;
    bool refreshedScreenAlready = false;

    QImage *image = nullptr;
};
