#ifndef TESLACLIENT_H
#define TESLACLIENT_H

#include <QObject>
#include <QStringList>
#include <QVariantList>

// Forward declare the opaque cbindgen handle from helper/electriceelcore.h.
struct Core;
class QTimer;

// Worker object that lives on its own QThread (see TeslaClient::setupWorker).
// Every blocking C ABI call (core_run/core_pair can take up to ~10 minutes)
// runs here so the GUI thread never stalls; results cross back through the
// queued signal connections TeslaClient wires in its constructor.
class CoreWorker : public QObject
{
    Q_OBJECT
public:
    explicit CoreWorker(QObject *parent = nullptr);
    ~CoreWorker() override;

public slots:
    // Mirrors TeslaClient's public slots, minus the QVariantList marshaling.
    void initialize(const QString &binDir, const QString &stateDir, const QString &sessionBin);
    void runCommand(const QString &requestId, const QString &cmd, const QStringList &args);
    void generateKey(bool force);
    void pair();
    void setConfig(const QString &vin, const QString &model, const QString &keyName,
                   int connectTimeoutSec, int commandTimeoutSec);
    void refreshConfig();
    void pollPhoneKeyEvents();
    void handleResume();
    void previewDestination(const QString &requestId, const QString &text);
    void shareDestination(const QString &requestId, const QString &text);
    void shutdown();

signals:
    void initialized(bool ok, const QString &errorMessage);
    void commandFinished(const QString &requestId, bool ok, const QString &stdOut,
                         const QString &stdErr, int exitCode);
    void commandError(const QString &requestId, const QString &message);
    void keyGenerated(bool ok, const QString &publicKeyPem, const QString &errorMessage);
    void paired(bool ok, const QString &output, const QString &errorMessage);
    void configSaved(bool ok, const QString &errorMessage);
    void configLoaded(const QString &vin, const QString &model, const QString &keyName,
                      int connectTimeoutSec, int commandTimeoutSec,
                      bool hasKey, const QString &publicKeyPem);
    void phoneKeyStarted(bool active, const QString &errorMessage);
    void phoneKeyEvent(const QString &kind, const QString &vin,
                       const QString &time, const QString &errorMessage);
    void destinationPreviewed(const QString &requestId, bool ok, const QString &kind,
                              const QString &value1, const QString &value2,
                              const QString &errorMessage);
    void shareFinished(const QString &requestId, bool ok, const QString &output,
                       const QString &errorMessage);

private:
    Core *m_core;
    QTimer *m_phoneKeyTimer;
};

// In-process client for the Rust control core (see docs/architecture.md for
// why). Previously this was the async D-Bus client for the privileged
// org.electriceel.Helper system service; the service is gone and the core is
// linked in via ffld's C ABI (helper/ffi.rs). The QML-facing surface (slots +
// signals + properties) is deliberately unchanged so the UI needed no edits
// during the migration.
class TeslaClient : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool helperAvailable READ helperAvailable NOTIFY helperAvailableChanged)
    Q_PROPERTY(QString appVersion READ appVersion CONSTANT)
    // core_version() from the in-process library; equal to APP_VERSION for a
    // matched build, so the UI's "version mismatch" banner stays quiet.
    Q_PROPERTY(QString helperVersion READ helperVersion NOTIFY helperVersionChanged)
    Q_PROPERTY(QString phoneKeyStatus READ phoneKeyStatus NOTIFY phoneKeyStatusChanged)

public:
    explicit TeslaClient(QObject *parent = nullptr);
    ~TeslaClient() override;

    bool helperAvailable() const;
    QString appVersion() const;
    QString helperVersion() const;
    QString phoneKeyStatus() const;

public slots:
    // requestId is caller-chosen and echoed back on commandFinished/
    // commandError so QML can match replies to the triggering control.
    void runCommand(const QString &requestId, const QString &cmd, const QVariantList &args);
    void previewDestination(const QString &requestId, const QString &text);
    void shareDestination(const QString &requestId, const QString &text);
    void generateKey(bool force);
    void pair();
    void setConfig(const QString &vin, const QString &model, const QString &keyName,
                   int connectTimeoutSec, int commandTimeoutSec);
    void refreshConfig();
    void refreshHelperAvailable();
    void refreshHelperVersion();

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
    void destinationPreviewed(const QString &requestId, bool ok, const QString &kind,
                              const QString &value1, const QString &value2,
                              const QString &errorMessage);
    void shareFinished(const QString &requestId, bool ok, const QString &output,
                       const QString &errorMessage);

private slots:
    void onInitialized(bool ok, const QString &errorMessage);
    void onPhoneKeyStarted(bool active, const QString &errorMessage);
    void onPhoneKeyEvent(const QString &kind, const QString &vin,
                         const QString &time, const QString &errorMessage);
    void onApplicationStateChanged(Qt::ApplicationState state);

private:
    void setHelperAvailable(bool available);

    CoreWorker *m_worker;
    bool m_helperAvailable;
    QString m_helperVersion;
    QString m_phoneKeyStatus;
    bool m_suspended = false;
};

#endif // TESLACLIENT_H