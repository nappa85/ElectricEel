#include "teslaclient.h"
#include "commandarguments.h"

#include <QDateTime>
#include <QDebug>
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLocale>
#include <QStringList>
#include <QStandardPaths>
#include <QUuid>

extern "C" {
#include "electriceelcore.h"
}

TeslaClient::TeslaClient(Runtime *runtime, QObject *parent)
    : QObject(parent), m_runtime(runtime), m_helperVersion(QString::fromUtf8(core_version()))
{
    m_monotonic.start();
    m_wallMinusMonotonic = QDateTime::currentMSecsSinceEpoch();
    runtime_observe(m_runtime, &TeslaClient::notification, this);
    connect(qApp, &QGuiApplication::applicationStateChanged,
            this, &TeslaClient::onApplicationStateChanged);
    onApplicationStateChanged(QGuiApplication::applicationState());
}

TeslaClient::~TeslaClient()
{
    // Rust guarantees no callback can use this QObject after detachment returns.
    // Qt discards queued deliveries automatically when the QObject is destroyed.
    runtime_observe(m_runtime, nullptr, nullptr);
}

QString TeslaClient::newRequestId(const QString &prefix, const QString &vin) const
{
    // Sailfish's Qt predates QUuid::WithoutBraces; trim the canonical braces.
    return prefix + QStringLiteral("#") + QUuid::createUuid().toString().mid(1, 36)
        + QStringLiteral(":") + vin;
}

void TeslaClient::notification(void *context, const char *json)
{
    auto *client = static_cast<TeslaClient *>(context);
    // Copy the borrowed data while the callback is live. All property changes
    // and signals must happen on Qt's GUI thread, never on Rust's worker.
    QMetaObject::invokeMethod(client, "deliverNotification", Qt::QueuedConnection,
                              Q_ARG(QByteArray, QByteArray(json)));
}

void TeslaClient::deliverNotification(const QByteArray &json)
{
    const QJsonObject event = QJsonDocument::fromJson(json).object();
    const QString type = event.value("type").toString();
    const QString id = event.value("request_id").toString();
    const QString error = event.value("error").toString();
    const bool ok = event.value("ok").toBool();
    if (type == "initialized") {
        // Snapshot acknowledgement; phone-key state follows separately.
    } else if (type == "phone_key_state") {
        const bool active = event.value("active").toBool();
        const QString status = event.value("status").toString();
        QString link = event.value("link").toString();
        const QStringList links = {QStringLiteral("unpaired"), QStringLiteral("bluetooth-off"),
            QStringLiteral("scanning"), QStringLiteral("connected"), QStringLiteral("authorized"),
            QStringLiteral("stopped"), QStringLiteral("error")};
        if (!links.contains(link)) {
            qWarning() << "TeslaClient: unknown phone-key link" << link;
            link = QStringLiteral("error");
        }
        const bool activeChanged = m_phoneKeyActive != active;
        const bool statusChanged = m_phoneKeyStatus != status;
        const bool linkChanged = m_phoneKeyLink != link;
        m_phoneKeyActive = active;
        m_phoneKeyStatus = status;
        m_phoneKeyLink = link;
        if (activeChanged) emit phoneKeyActiveChanged();
        if (statusChanged) emit phoneKeyStatusChanged();
        if (linkChanged) emit phoneKeyLinkChanged();
        if (activeChanged || statusChanged || linkChanged)
            emit phoneKeyStateChanged(active, link, status);
    } else if (type == "phone_key_event") {
        emit phoneKeyEvent(event.value("kind").toString(), event.value("vin").toString(),
                          event.value("time").toString(), error);
    } else if (type == "command_finished") {
        emit commandFinished(id, ok, event.value("stdout").toString(),
                             event.value("stderr").toString(), event.value("exit_code").toInt());
    } else if (type == "command_error") {
        emit commandError(id, error);
    } else if (type == "key_generated") {
        emit keyGenerated(ok, event.value("pem").toString(), error);
    } else if (type == "paired") {
        emit paired(ok, event.value("output").toString(), error);
    } else if (type == "config_saved") {
        emit configSaved(ok, error);
    } else if (type == "automagic_export_finished") {
        m_automagicExportBusy = false;
        emit automagicExportBusyChanged();
        emit automagicExportFinished(ok, event.value("path").toString(), error);
    } else if (type == "config_loaded") {
        applyConfig(event);
        emit configLoaded(event.value("vin").toString(), event.value("model").toString(),
                          event.value("key_name").toString(), event.value("connect_timeout_sec").toInt(),
                          event.value("command_timeout_sec").toInt(), event.value("has_key").toBool(),
                          event.value("pem").toString(),
                          event.value("cover_left_action").toString(),
                          event.value("cover_right_action").toString());
    } else if (type == "destination_previewed") {
        emit destinationPreviewed(id, ok, event.value("kind").toString(),
                                  event.value("value1").toString(), event.value("value2").toString(), error);
    } else if (type == "share_finished") {
        emit shareFinished(id, ok, event.value("output").toString(), error);
    } else if (type == "status_refresh_requested") {
        emit statusRefreshRequested();
    } else if (!type.isEmpty()) {
        qWarning() << "TeslaClient: unknown notification type" << type;
    } else {
        qWarning() << "TeslaClient: notification without type" << json;
    }
}

void TeslaClient::submit(const QJsonObject &request)
{
    const QByteArray json = QJsonDocument(request).toJson(QJsonDocument::Compact);
    if (runtime_submit(m_runtime, json.constData()))
        return;
    const QString error = QStringLiteral("Control runtime unavailable or request queue full");
    const QString op = request.value("op").toString();
    const QString id = request.value("request_id").toString();
    if (op == "generate_key") emit keyGenerated(false, QString(), error);
    else if (op == "pair") emit paired(false, QString(), error);
    else if (op == "set_config") emit configSaved(false, error);
    else if (op == "preview_destination") emit destinationPreviewed(id, false, QString(), QString(), QString(), error);
    else if (op == "share_destination") emit shareFinished(id, false, QString(), error);
    else if (op == "run") emit commandError(id, error);
    else if (op == "get_config") {
        setConfigError(error);
        emit configLoadError(error);
    }
    else if (op == "export_automagic") {
        m_automagicExportBusy = false;
        emit automagicExportBusyChanged();
        emit automagicExportFinished(false, QString(), error);
    }
    else qWarning() << error << op;
}

void TeslaClient::applyConfig(const QJsonObject &event)
{
    // Assign everything before notifying, so a handler reacting to one
    // change (e.g. vinChanged) already sees the rest of the new reply.
    const QString vin = event.value("vin").toString();
    const QString model = event.value("model").toString();
    const bool hasKey = event.value("has_key").toBool();
    const QString pem = hasKey ? event.value("pem").toString() : QString();
    const QString left = event.value("cover_left_action").toString(QStringLiteral("frunk-open"));
    const QString right = event.value("cover_right_action").toString(QStringLiteral("trunk-open"));
    const QString vehicleName = event.value("vehicle_name").toString();
    const bool notificationEnabled = event.value("notification_enabled").toBool();
    QStringList notificationActions;
    for (const QJsonValue &action : event.value("notification_actions").toArray())
        notificationActions.append(action.toString());
    const bool vinChanged = m_vin != vin;
    const bool modelChanged = m_model != model;
    const bool hasKeyChanged = m_hasKey != hasKey;
    const bool pemChanged = m_publicKeyPem != pem;
    const bool readyChanged = !m_configReady;
    const bool errorChanged = !m_configError.isEmpty();
    const bool coverChanged = m_coverLeftAction != left || m_coverRightAction != right;
    const bool vehicleNameChanged = m_vehicleName != vehicleName;
    const bool notificationChanged = m_notificationEnabled != notificationEnabled
        || m_notificationActions != notificationActions;
    m_vin = vin;
    m_model = model;
    m_hasKey = hasKey;
    m_publicKeyPem = pem;
    m_configReady = true;
    m_configError.clear();
    m_coverLeftAction = left;
    m_coverRightAction = right;
    m_vehicleName = vehicleName;
    m_notificationEnabled = notificationEnabled;
    m_notificationActions = notificationActions;
    if (vinChanged) emit this->vinChanged();
    if (modelChanged) emit this->modelChanged();
    if (hasKeyChanged) emit this->hasKeyChanged();
    if (pemChanged) emit publicKeyPemChanged();
    if (coverChanged) emit coverActionsChanged();
    if (vehicleNameChanged) emit this->vehicleNameChanged();
    if (notificationChanged) emit notificationSettingsChanged();
    if (errorChanged) emit configErrorChanged();
    if (readyChanged) emit configReadyChanged();
}

void TeslaClient::setConfigError(const QString &error)
{
    if (m_configError == error)
        return;
    m_configError = error;
    emit configErrorChanged();
}

QString TeslaClient::appVersion() const { return QString::fromLatin1(APP_VERSION); }
QString TeslaClient::phoneKeyStatus() const { return m_phoneKeyStatus; }

void TeslaClient::runCommand(const QString &requestId, const QString &cmd, const QVariantList &args,
                             bool refreshStatus)
{
    QJsonArray values;
    // Numbers must serialize with a C locale: QVariant::toString() follows
    // the system locale (e.g. "48,8584" in Italian), which Go's ParseFloat
    // rejects. Dialog values are already strings; this only guards direct
    // numeric passes.
    for (const QVariant &arg : args) {
        values.append(commandArgument(arg));
    }
    submit({{"op", "run"}, {"request_id", requestId}, {"cmd", cmd}, {"args", values},
            {"refresh_status", refreshStatus}});
}

void TeslaClient::generateKey(bool force) { submit({{"op", "generate_key"}, {"force", force}}); }
void TeslaClient::pair() { submit({{"op", "pair"}}); }

void TeslaClient::setConfig(const QString &vin, const QString &model, const QString &keyName,
                            int connectTimeoutSec, int commandTimeoutSec,
                            const QString &coverLeftAction, const QString &coverRightAction)
{
    submit({{"op", "set_config"}, {"vin", vin}, {"model", model}, {"key_name", keyName},
            {"connect_timeout_sec", connectTimeoutSec}, {"command_timeout_sec", commandTimeoutSec},
            {"cover_left_action", coverLeftAction}, {"cover_right_action", coverRightAction}});
}

void TeslaClient::setNotificationSettings(bool enabled, const QStringList &actions)
{
    submit({{"op", "set_notification"}, {"enabled", enabled},
            {"actions", QJsonArray::fromStringList(actions)}});
}

void TeslaClient::refreshConfig() { submit({{"op", "get_config"}}); }

void TeslaClient::previewDestination(const QString &requestId, const QString &text)
{
    submit({{"op", "preview_destination"}, {"request_id", requestId}, {"text", text}});
}

void TeslaClient::shareDestination(const QString &requestId, const QString &text)
{
    submit({{"op", "share_destination"}, {"request_id", requestId}, {"text", text}});
}

void TeslaClient::logPowerState(const QString &state) { submit({{"op", "log_ui"}, {"message", state}}); }

void TeslaClient::onApplicationStateChanged(Qt::ApplicationState state)
{
    // Sailfish reports only Active/Inactive, never ApplicationSuspended, so
    // Rust's suspended->active resume recovery would never run. A system
    // suspend stops the monotonic clock but not wall time: when the gap grew
    // since the previous state change, report the suspend Rust expects.
    const qint64 wallMinusMonotonic = QDateTime::currentMSecsSinceEpoch() - m_monotonic.elapsed();
    const bool slept = wallMinusMonotonic - m_wallMinusMonotonic > 10000;
    m_wallMinusMonotonic = wallMinusMonotonic;
    if (slept && state != Qt::ApplicationSuspended)
        submit({{"op", "application_state"}, {"state", "suspended"}});

    const char *name = "unknown";
    switch (state) {
    case Qt::ApplicationActive: name = "active"; break;
    case Qt::ApplicationInactive: name = "inactive"; break;
    case Qt::ApplicationHidden: name = "hidden"; break;
    case Qt::ApplicationSuspended: name = "suspended"; break;
    }
    submit({{"op", "application_state"}, {"state", name}});
}

void TeslaClient::exportAutomagicFlows()
{
    if (m_automagicExportBusy)
        return;
    m_automagicExportBusy = true;
    emit automagicExportBusyChanged();
    submit({{"op", "export_automagic"},
            {"documents_dir", QStandardPaths::writableLocation(QStandardPaths::DocumentsLocation)}});
}
