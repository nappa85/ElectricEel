#include "../src/teslaclient.h"
#include <QGuiApplication>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QThread>
#include <QtTest>

extern "C" {
#include "electriceelcore.h"
}

// Link the actual Rust entrypoint and runtime to the actual Qt adapter, replacing
// only the Sailfish view callbacks with this headless test UI.
int main(int argc, char *argv[])
{
    return electric_eel_app_main(argc, argv);
}

static QGuiApplication *application = nullptr;
static Runtime *runtime = nullptr;
static QTemporaryDir directory;
static QByteArray stateDirectory;

class RustRuntimeTest : public QObject
{
    Q_OBJECT
private slots:
    void rustPushesResultsToTheGuiThread() {
        TeslaClient client(runtime);
        QSignalSpy configs(&client, &TeslaClient::configLoaded);
        client.refreshConfig();
        QTRY_COMPARE(configs.count(), 1);
        QCOMPARE(configs.first().at(3).toInt(), 20);
        QVERIFY(!configs.first().at(5).toBool());
        QTRY_VERIFY(!client.phoneKeyStatus().isEmpty());
        QVERIFY(!client.phoneKeyActive());
        QTRY_COMPARE(client.phoneKeyLink(), QStringLiteral("unpaired"));

        QSignalSpy errors(&client, &TeslaClient::commandError);
        const QString id = QString::fromUtf8("quoted-\"-città");
        client.runCommand(id, "not-a-command", {});
        QTRY_COMPARE(errors.count(), 1);
        QCOMPARE(errors.first().at(0).toString(), id);

        QSignalSpy previews(&client, &TeslaClient::destinationPreviewed);
        client.previewDestination("navigation", "geo:45,9");
        QTRY_COMPARE(previews.count(), 1);
        QVERIFY(previews.first().at(1).toBool());
        QCOMPARE(previews.first().at(2).toString(), QString("gps"));

        QSignalSpy refreshes(&client, &TeslaClient::statusRefreshRequested);
        bool guiThread = false;
        connect(&client, &TeslaClient::statusRefreshRequested, this, [&guiThread] {
            guiThread = QThread::currentThread() == qApp->thread();
        });
        client.runCommand("toggle-confirmation", "not-a-command", {}, true);
        QThread::msleep(2800); // Rust's deadline expires without GUI processing.
        QCOMPARE(refreshes.count(), 0); // Qt delivery must remain queued.
        QTRY_COMPARE(refreshes.count(), 1);
        QVERIFY(guiThread);
    } // Detaches callback before the Rust entrypoint destroys its runtime.

    void phoneKeyPropertiesAreCoherentAndProseIsDisplayOnly() {
        TeslaClient client(runtime);
        bool coherent = false;
        connect(&client, &TeslaClient::phoneKeyActiveChanged, this, [&] {
            coherent = client.phoneKeyActive()
                && client.phoneKeyLink() == QStringLiteral("authorized")
                && client.phoneKeyStatus() == QStringLiteral("arbitrary diagnostic");
        });
        QVERIFY(QMetaObject::invokeMethod(&client, "deliverNotification", Qt::DirectConnection,
            Q_ARG(QByteArray, QByteArray(R"({"type":"phone_key_state","active":true,"link":"authorized","status":"arbitrary diagnostic"})"))));
        QVERIFY(coherent);
        QVERIFY(QMetaObject::invokeMethod(&client, "deliverNotification", Qt::DirectConnection,
            Q_ARG(QByteArray, QByteArray(R"({"type":"phone_key_state","active":true,"link":"future-value","status":"Phone key connected"})"))));
        QCOMPARE(client.phoneKeyLink(), QStringLiteral("error"));
        QCOMPARE(client.phoneKeyStatus(), QStringLiteral("Phone key connected"));
    }
};

extern "C" const char *electric_eel_ui_prepare(int argc, char **argv)
{
    static int qtArgc;
    qtArgc = argc;
    qputenv("QT_QPA_PLATFORM", "minimal");
    application = new QGuiApplication(qtArgc, argv);
    if (!directory.isValid()) return nullptr;
    stateDirectory = directory.path().toUtf8();
    qputenv("ELECTRIC_EEL_LOG_DIR", stateDirectory);
    return stateDirectory.constData();
}

extern "C" int electric_eel_ui_run(Runtime *handle)
{
    runtime = handle;
    RustRuntimeTest test;
    return QTest::qExec(&test);
}

extern "C" void electric_eel_ui_cleanup()
{
    delete application;
    application = nullptr;
}

#include "rustruntime_test.moc"
