#ifndef TESLACLIENT_H
#define TESLACLIENT_H

#include <QObject>
#include <QVariantList>
#include <QJsonObject>

struct Runtime;

// Qt-only adapter: marshal UI actions and render Rust's pushed notifications.
// No worker, timers, BLE lifecycle decisions or power-management logic.
class TeslaClient : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool helperAvailable READ helperAvailable NOTIFY helperAvailableChanged)
    Q_PROPERTY(QString appVersion READ appVersion CONSTANT)
    // core_version() from the in-process library; equal to APP_VERSION for a
    // matched build, so the UI's "version mismatch" banner stays quiet.
    Q_PROPERTY(QString helperVersion READ helperVersion NOTIFY helperVersionChanged)
    Q_PROPERTY(QString phoneKeyStatus READ phoneKeyStatus NOTIFY phoneKeyStatusChanged)
    Q_PROPERTY(bool phoneKeyActive READ phoneKeyActive NOTIFY phoneKeyActiveChanged)

public:
    explicit TeslaClient(Runtime *runtime, QObject *parent = nullptr);
    ~TeslaClient() override;

    bool helperAvailable() const;
    QString appVersion() const;
    QString helperVersion() const;
    QString phoneKeyStatus() const;
    bool phoneKeyActive() const { return m_phoneKeyActive; }

public slots:
    // requestId is caller-chosen and echoed back on commandFinished/
    // commandError so QML can match replies to the triggering control.
    void runCommand(const QString &requestId, const QString &cmd, const QVariantList &args,
                    bool refreshStatus = false);
    void previewDestination(const QString &requestId, const QString &text);
    void shareDestination(const QString &requestId, const QString &text);
    void generateKey(bool force);
    void pair();
    void setConfig(const QString &vin, const QString &model, const QString &keyName,
                   int connectTimeoutSec, int commandTimeoutSec);
    void refreshConfig();
    void refreshHelperAvailable();
    void refreshHelperVersion();
    void logPowerState(const QString &state);

signals:
    void commandFinished(const QString &requestId, bool ok, const QString &stdOut,
                         const QString &stdErr, int exitCode);
    void commandError(const QString &requestId, const QString &message);
    void keyGenerated(bool ok, const QString &publicKeyPem, const QString &errorMessage);
    void paired(bool ok, const QString &output, const QString &errorMessage);
    void configSaved(bool ok, const QString &errorMessage);
    void configLoaded(const QString &vin, const QString &model, const QString &keyName,
                      int connectTimeoutSec, int commandTimeoutSec,
                      bool hasKey, const QString &publicKeyPem);
    void helperAvailableChanged();
    void helperVersionChanged();
    void phoneKeyStatusChanged();
    void phoneKeyActiveChanged();
    void statusRefreshRequested();
    void phoneKeyEvent(const QString &kind, const QString &vin,
                       const QString &time, const QString &errorMessage);
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
    static void notification(void *context, const char *json);
    Runtime *m_runtime;
    bool m_helperAvailable = true;
    QString m_helperVersion;
    QString m_phoneKeyStatus;
    bool m_phoneKeyActive = false;
};

#endif // TESLACLIENT_H
