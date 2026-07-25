#include "FBController.h"
#include "fbmanagement.h"
#include "qtfb_log.h"

#include <QtCore/QMetaObject>
#include <QtGui/QKeyEvent>
#include <QtGui/QMouseEvent>
#include <QtGui/QTouchEvent>
#include <QtGui/QTabletEvent>

FBController::FBController(QQuickItem *parent)
    : QQuickPaintedItem(parent)
{
    setAcceptTouchEvents(true);
    // Upstream casts 0xFFFFFFFF; Qt::AllButtons is the same intent without UB.
    setAcceptedMouseButtons(Qt::AllButtons);
    // QQuickItem::setFocusPolicy() only exists from Qt 6.7; upstream targets a
    // newer Qt than the rM2's 6.5.2. The 6.5 equivalent is tab focus + an
    // explicit forceActiveFocus() when we go active (see setActive()).
    setActiveFocusOnTab(true);

    // rM2 port: the i.MX7 has no 3D GPU, so xochitl's scene graph runs on the
    // software/e-ink adaptation. FramebufferObject render targets are invalid
    // there -- keep the raster Image target (which is also the Qt 6 default).
    setRenderTarget(QQuickPaintedItem::Image);
    // We always cover our whole bounding rect with an opaque RGB16 image.
    setOpaquePainting(true);
    setAntialiasing(false);

    if (_autoHide) {
        setVisible(false);
        setEnabled(false);
    }

    // Nothing downstream of us can drive an rM2 waveform directly, so the only
    // meaningful response to a full-refresh request is to repaint everything.
    connect(this, &FBController::requestFullRefresh, this, [this]() { update(); });
}

FBController::~FBController()
{
    qtfb::management::unregisterController(_framebufferID);
}

void FBController::setFramebufferID(int fbId)
{
    if (_framebufferID != fbId && _framebufferID != -1) {
        QDEBUG << "Re-register framebuffer" << _framebufferID << "as" << fbId;
        qtfb::management::unregisterController(_framebufferID);
    }
    _framebufferID = fbId;
    qtfb::management::registerController(fbId, QPointer<FBController>(this));
}

int FBController::framebufferID() const { return _framebufferID; }

bool FBController::active() const { return _active; }

void FBController::setActive(bool active)
{
    _active = active;
    if (_autoHide) {
        setVisible(active);
        setEnabled(active);
        if (active)
            forceActiveFocus();
    }
    emit activeChanged();
    markedUpdate();
}

void FBController::paint(QPainter *painter)
{
    isMidPaint = true;
    if (this->image && this->_active) {
        if (_allowScaling) {
            painter->drawImage(QRectF(0, 0, width(), height()), *image, image->rect());
        } else {
            painter->drawImage(0, 0, *image);
        }
    }
    isMidPaint = false;
}

void FBController::associateSHM(QImage *img)
{
    this->image = img;
    const int key = _framebufferID;
    // Bounce onto the GUI thread: this is called from a server worker thread.
    QMetaObject::invokeMethod(this, [this, key]() {
        if (!qtfb::management::isControllerAssociated(key)) {
            // The connection went away while we waited for the event loop.
            this->image = nullptr;
            this->setActive(false);
            return;
        }
        this->setActive(this->image != nullptr);
    }, Qt::QueuedConnection);
}

void FBController::markedUpdate(const QRect &rect)
{
    if (rect.isNull()) {
        // Do NOT forward a null rect to QQuickPaintedItem::update(): when its
        // internal dirty rect is still empty (i.e. the very first update after
        // the item is created), a null rect is a no-op and the next paint only
        // covers whatever a later partial update dirtied -- leaving the rest of
        // the item as freshly-allocated black. Verified in the native harness:
        // an UPDATE_ALL followed by an UPDATE_PARTIAL(0,0,400,400) rendered
        // only the 400x400 corner and left the other ~95% of the screen black.
        // Passing the explicit bounding rect makes UPDATE_ALL mean all.
        update(contentsBoundingRect().toAlignedRect());
        return;
    }
    if (_allowScaling && image && image->width() > 0 && image->height() > 0) {
        // Upstream does `(rect.x() / image->width()) * width()` in integer
        // arithmetic, which truncates to 0 for every realistic rect. Multiply
        // first.
        const qreal sx = width() / (qreal) image->width();
        const qreal sy = height() / (qreal) image->height();
        update(QRect((int) (rect.x() * sx),
                     (int) (rect.y() * sy),
                     (int) (rect.width() * sx) + 1,
                     (int) (rect.height() * sy) + 1));
    } else {
        update(rect);
    }
}

void FBController::setAllowScaling(bool a) { _allowScaling = a; }
bool FBController::allowScaling() const { return _allowScaling; }

void FBController::setAutoHide(bool a)
{
    _autoHide = a;
    if (_autoHide) {
        setVisible(_active);
        setEnabled(_active);
    } else {
        setVisible(true);
        setEnabled(true);
    }
}
bool FBController::autoHide() const { return _autoHide; }

QPoint FBController::convertPointToQTFBPixels(const QPointF &input) const
{
    if (_allowScaling && image && width() > 0 && height() > 0) {
        return QPoint((int) (input.x() * image->width() / width()),
                      (int) (input.y() * image->height() / height()));
    }
    return QPoint((int) input.x(), (int) input.y());
}

void FBController::sendPointer(int type, const QPointF &pos, int devId, int pressure)
{
    if (_framebufferID == -1)
        return;
    const QPoint conv = convertPointToQTFBPixels(pos);
    qtfb::UserInputContents packet {};
    packet.inputType = type;
    packet.devId = devId;
    packet.x = conv.x();
    packet.y = conv.y();
    packet.d = pressure;
    qtfb::management::forwardUserInput(_framebufferID, &packet);
}

void FBController::mousePressEvent(QMouseEvent *me)
{
    if (!me->points().isEmpty()) {
        const QEventPoint &point = me->points().at(0);
        sendPointer(INPUT_PEN_PRESS, point.position(), 0, (int) (point.pressure() * 100.0));
    }
    me->accept();
}

void FBController::mouseMoveEvent(QMouseEvent *me)
{
    if (!me->points().isEmpty()) {
        const QEventPoint &point = me->points().at(0);
        sendPointer(INPUT_PEN_UPDATE, point.position(), 0, (int) (point.pressure() * 100.0));
    }
    me->accept();
}

void FBController::mouseReleaseEvent(QMouseEvent *me)
{
    sendPointer(INPUT_PEN_RELEASE, me->position(), 0, 0);
    me->accept();
}

bool FBController::event(QEvent *ev)
{
    switch (ev->type()) {
    case QEvent::TabletPress:
    case QEvent::TabletMove:
    case QEvent::TabletRelease: {
        auto *te = static_cast<QTabletEvent *>(ev);
        int t = INPUT_PEN_UPDATE;
        if (ev->type() == QEvent::TabletPress)
            t = INPUT_PEN_PRESS;
        else if (ev->type() == QEvent::TabletRelease)
            t = INPUT_PEN_RELEASE;
        // devId 1 == eraser, 0 == stylus tip.
        const int devId = (te->pointerType() == QPointingDevice::PointerType::Eraser) ? 1 : 0;
        sendPointer(t, te->position(), devId, (int) (te->pressure() * 100.0));
        ev->accept();
        return true;
    }
    default:
        break;
    }
    return QQuickPaintedItem::event(ev);
}

static inline void sendKeyEvent(int key, int pkt, qtfb::FBKey framebufferID)
{
    if (framebufferID == -1)
        return;
    qtfb::UserInputContents packet {};
    packet.inputType = pkt;
    packet.devId = 0;
    packet.x = key;
    packet.y = 0;
    packet.d = 0;
    qtfb::management::forwardUserInput(framebufferID, &packet);
}

void FBController::virtualKeyboardKeyDown(int key) { sendKeyEvent(key, INPUT_VKB_PRESS, _framebufferID); }
void FBController::virtualKeyboardKeyUp(int key)   { sendKeyEvent(key, INPUT_VKB_RELEASE, _framebufferID); }
void FBController::specialKeyDown(int key)         { sendKeyEvent(key, INPUT_BTN_PRESS, _framebufferID); }
void FBController::specialKeyUp(int key)           { sendKeyEvent(key, INPUT_BTN_RELEASE, _framebufferID); }

void FBController::touchEvent(QTouchEvent *me)
{
    if (_framebufferID != -1) {
        const int lenPoints = (int) me->points().length();
        if (lenPoints == 5 && !refreshedScreenAlready) {
            emit requestFullRefresh();
            refreshedScreenAlready = true;
            QDEBUG << "QTFB force refresh (5-finger)";
        }
        for (const QEventPoint &point : me->points()) {
            const QPoint conv = convertPointToQTFBPixels(point.position());
            int type = INPUT_TOUCH_PRESS;
            switch (point.state()) {
            case QEventPoint::State::Pressed:
                type = INPUT_TOUCH_PRESS;
                if (conv.y() < 100)
                    checkingGestureDragDown = true;
                break;
            case QEventPoint::State::Released:
                type = INPUT_TOUCH_RELEASE;
                if (conv.y() > 100 && conv.y() < 400 && checkingGestureDragDown)
                    emit dragDown();
                if (lenPoints == 1)
                    refreshedScreenAlready = false;
                checkingGestureDragDown = false;
                break;
            case QEventPoint::State::Updated:
                type = INPUT_TOUCH_UPDATE;
                break;
            default:
                break;
            }
            qtfb::UserInputContents packet {};
            packet.inputType = type;
            packet.devId = point.id();
            packet.x = conv.x();
            packet.y = conv.y();
            packet.d = 0;
            qtfb::management::forwardUserInput(_framebufferID, &packet);
        }
    }
    me->accept();
}

static inline int translateKey(int qtKey)
{
    switch (qtKey) {
    case Qt::Key_Right: return INPUT_BTN_X_RIGHT;
    case Qt::Key_Left:  return INPUT_BTN_X_LEFT;
    case Qt::Key_Home:  return INPUT_BTN_X_HOME;
    default: break;
    }
    return -1;
}

void FBController::keyPressEvent(QKeyEvent *ke)
{
    const int k = translateKey(ke->key());
    if (k != -1)
        specialKeyDown(k);
}

void FBController::keyReleaseEvent(QKeyEvent *ke)
{
    const int k = translateKey(ke->key());
    if (k != -1)
        specialKeyUp(k);
}

int FBController::refreshMode() const { return _refreshMode; }

void FBController::setRefreshMode(int rm)
{
    // NOTE (rM2): SWTCON is statically linked inside xochitl and exposes no
    // per-item waveform hint, so this property is currently advisory only --
    // it is stored and signalled but does not change the panel waveform.
    if (rm != _refreshMode) {
        _refreshMode = rm;
        emit refreshModeChanged();
    }
}
