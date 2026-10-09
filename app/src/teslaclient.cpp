#include "teslaclient.h"
#include "commandarguments.h"

#include <QDebug>
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLocale>
#include <QStringList>

extern "C" {
#include "electriceelcore.h"
}

TeslaClient::TeslaClient(Runtime *runtime, QObject *parent)
    : QObject(parent), m_runtime(runtime), m_helperVersion(QString::fromUtf8(core_version()))
{
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
        if (m_helperAvailable != ok) {
            m_helperAvailable = ok;
            emit helperAvailableChanged();
        }
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
    } else if (type == "config_loaded") {
        emit configLoaded(event.value("vin").toString(), event.value("model").toString(),
                          event.value("key_name").toString(), event.value("connect_timeout_sec").toInt(),
                          event.value("command_timeout_sec").toInt(), event.value("has_key").toBool(),
                          event.value("pem").toString());
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
    else if (op == "get_config") emit configLoadError(error);
    else if (op == "log_ui" || op == "application_state") { qWarning() << error << op; }
    else qWarning() << error << op;
}

bool TeslaClient::helperAvailable() const { return m_helperAvailable; }
QString TeslaClient::appVersion() const { return QString::fromLatin1(APP_VERSION); }
QString TeslaClient::helperVersion() const { return m_helperVersion; }
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
                            int connectTimeoutSec, int commandTimeoutSec)
{
    submit({{"op", "set_config"}, {"vin", vin}, {"model", model}, {"key_name", keyName},
            {"connect_timeout_sec", connectTimeoutSec}, {"command_timeout_sec", commandTimeoutSec}});
}

void TeslaClient::refreshConfig() { submit({{"op", "get_config"}}); }
void TeslaClient::refreshHelperAvailable() { emit helperAvailableChanged(); }
void TeslaClient::refreshHelperVersion() { emit helperVersionChanged(); }

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
    const char *name = "unknown";
    switch (state) {
    case Qt::ApplicationActive: name = "active"; break;
    case Qt::ApplicationInactive: name = "inactive"; break;
    case Qt::ApplicationHidden: name = "hidden"; break;
    case Qt::ApplicationSuspended: name = "suspended"; break;
    }
    submit({{"op", "application_state"}, {"state", name}});
}
