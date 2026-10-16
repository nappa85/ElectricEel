#ifndef TESLACLIENT_H
#define TESLACLIENT_H

#include <QElapsedTimer>
#include <QObject>
#include <QStringList>
#include <QVariantList>
#include <QJsonObject>

struct Runtime;

// Qt-only adapter: marshal UI actions and render Rust's pushed notifications.
// No worker, timers, BLE lifecycle decisions or power-management logic.
class TeslaClient : public QObject
{
    Q_OBJECT
    Q_PROPERTY(QString appVersion READ appVersion CONSTANT)
    // core_version() from the statically linked core; equal to APP_VERSION
    // for a matched build, so the UI's "version mismatch" warning stays quiet.
    Q_PROPERTY(QString helperVersion READ helperVersion CONSTANT)
    // The persisted configuration, owned here so pages bind to one copy
    // instead of each caching broadcast replies. Change signals fire only
    // when a value actually changes; configLoaded still reports every reply.
    Q_PROPERTY(QString vin READ vin NOTIFY vinChanged)
    Q_PROPERTY(QString model READ model NOTIFY modelChanged)
    Q_PROPERTY(bool hasKey READ hasKey NOTIFY hasKeyChanged)
    Q_PROPERTY(QString publicKeyPem READ publicKeyPem NOTIFY publicKeyPemChanged)
    // True once any configuration reply arrived. A later failed reload keeps
    // the last good values and only sets configError.
    Q_PROPERTY(bool configReady READ configReady NOTIFY configReadyChanged)
    Q_PROPERTY(QString configError READ configError NOTIFY configErrorChanged)
    Q_PROPERTY(QString coverLeftAction READ coverLeftAction NOTIFY coverActionsChanged)
    Q_PROPERTY(QString coverRightAction READ coverRightAction NOTIFY coverActionsChanged)
    // The car's own name as last read from / written to the vehicle.
    Q_PROPERTY(QString vehicleName READ vehicleName NOTIFY vehicleNameChanged)
    Q_PROPERTY(bool notificationEnabled READ notificationEnabled NOTIFY notificationSettingsChanged)
    Q_PROPERTY(QStringList notificationActions READ notificationActions NOTIFY notificationSettingsChanged)
    Q_PROPERTY(QString phoneKeyStatus READ phoneKeyStatus NOTIFY phoneKeyStatusChanged)
    Q_PROPERTY(bool phoneKeyActive READ phoneKeyActive NOTIFY phoneKeyActiveChanged)
    Q_PROPERTY(QString phoneKeyLink READ phoneKeyLink NOTIFY phoneKeyLinkChanged)
    Q_PROPERTY(bool automagicExportBusy READ automagicExportBusy NOTIFY automagicExportBusyChanged)

public:
    explicit TeslaClient(Runtime *runtime, QObject *parent = nullptr);
    ~TeslaClient() override;

    QString appVersion() const;
    QString helperVersion() const { return m_helperVersion; }
    QString vin() const { return m_vin; }
    QString model() const { return m_model; }
    bool hasKey() const { return m_hasKey; }
    QString publicKeyPem() const { return m_publicKeyPem; }
    bool configReady() const { return m_configReady; }
    QString configError() const { return m_configError; }
    QString coverLeftAction() const { return m_coverLeftAction; }
    QString coverRightAction() const { return m_coverRightAction; }
    QString vehicleName() const { return m_vehicleName; }
    bool notificationEnabled() const { return m_notificationEnabled; }
    QStringList notificationActions() const { return m_notificationActions; }
    QString phoneKeyStatus() const;
    bool phoneKeyActive() const { return m_phoneKeyActive; }
    QString phoneKeyLink() const { return m_phoneKeyLink; }
    bool automagicExportBusy() const { return m_automagicExportBusy; }

public slots:
    QString newRequestId(const QString &prefix, const QString &vin = QString()) const;
    // requestId is caller-chosen and echoed back on commandFinished/
    // commandError so QML can match replies to the triggering control.
    void runCommand(const QString &requestId, const QString &cmd, const QVariantList &args,
                    bool refreshStatus = false);
    void previewDestination(const QString &requestId, const QString &text);
    void shareDestination(const QString &requestId, const QString &text);
    void generateKey(bool force);
    void pair();
    void setConfig(const QString &vin, const QString &model, const QString &keyName,
                   int connectTimeoutSec, int commandTimeoutSec,
                   const QString &coverLeftAction, const QString &coverRightAction);
    // Applies the status-notification settings immediately and on their own.
    void setNotificationSettings(bool enabled, const QStringList &actions);
    void refreshConfig();
    void logPowerState(const QString &state);
    void exportAutomagicFlows();

signals:
    void commandFinished(const QString &requestId, bool ok, const QString &stdOut,
                         const QString &stdErr, int exitCode);
    void commandError(const QString &requestId, const QString &message);
    void keyGenerated(bool ok, const QString &publicKeyPem, const QString &errorMessage);
    void paired(bool ok, const QString &output, const QString &errorMessage);
    void configSaved(bool ok, const QString &errorMessage);
    void configLoadError(const QString &message);
    void configLoaded(const QString &vin, const QString &model, const QString &keyName,
                      int connectTimeoutSec, int commandTimeoutSec,
                      bool hasKey, const QString &publicKeyPem,
                      const QString &coverLeftAction, const QString &coverRightAction);
    void vinChanged();
    void modelChanged();
    void hasKeyChanged();
    void publicKeyPemChanged();
    void configReadyChanged();
    void configErrorChanged();
    void coverActionsChanged();
    void vehicleNameChanged();
    void notificationSettingsChanged();
    void phoneKeyStatusChanged();
    void phoneKeyActiveChanged();
    void phoneKeyLinkChanged();
    void phoneKeyStateChanged(bool active, const QString &link, const QString &status);
    void statusRefreshRequested();
    void phoneKeyEvent(const QString &kind, const QString &vin,
                       const QString &time, const QString &errorMessage);
    void automagicExportBusyChanged();
    void automagicExportFinished(bool ok, const QString &path, const QString &errorMessage);
    void destinationPreviewed(const QString &requestId, bool ok, const QString &kind,
                              const QString &value1, const QString &value2,
                              const QString &errorMessage);
    void shareFinished(const QString &requestId, bool ok, const QString &output,
                       const QString &errorMessage);

private slots:
    void deliverNotification(const QByteArray &json);
    void onApplicationStateChanged(Qt::ApplicationState state);

private:
    void submit(const QJsonObject &request);
    void applyConfig(const QJsonObject &event);
    void setConfigError(const QString &error);
    static void notification(void *context, const char *json);
    Runtime *m_runtime;
    QString m_helperVersion;
    QString m_vin;
    QString m_model;
    bool m_hasKey = false;
    QString m_publicKeyPem;
    bool m_configReady = false;
    QString m_configError;
    QString m_coverLeftAction = QStringLiteral("frunk-open");
    QString m_coverRightAction = QStringLiteral("trunk-open");
    QString m_vehicleName;
    bool m_notificationEnabled = false;
    QStringList m_notificationActions = {QStringLiteral("lock"), QStringLiteral("unlock")};
    // Detects system suspend between application-state changes: the
    // monotonic clock stops during suspend while wall time keeps running.
    QElapsedTimer m_monotonic;
    qint64 m_wallMinusMonotonic = 0;
    QString m_phoneKeyStatus;
    bool m_phoneKeyActive = false;
    QString m_phoneKeyLink = QStringLiteral("stopped");
    bool m_automagicExportBusy = false;
};

#endif // TESLACLIENT_H
