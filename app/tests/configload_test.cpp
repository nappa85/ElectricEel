#include "../src/teslaclient.h"
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <cassert>

extern "C" {
#include "runtime_stub/electriceelcore.h"

static bool acceptRequests = false;
static QJsonObject submitted;
bool runtime_submit(Runtime *, const char *json) {
    submitted = QJsonDocument::fromJson(json).object();
    return acceptRequests;
}
void runtime_observe(Runtime *, void (*)(void *, const char *), void *) {}
const char *core_version() { return "test"; }
}

int main(int argc, char **argv)
{
    qputenv("QT_QPA_PLATFORM", "minimal");
    QGuiApplication app(argc, argv);
    TeslaClient client(nullptr);
    int loaded = 0;
    int failures = 0;
    QObject::connect(&client, &TeslaClient::configLoaded, [&] { ++loaded; });
    QObject::connect(&client, &TeslaClient::configLoadError, [&](const QString &message) {
        assert(!message.isEmpty());
        ++failures;
    });
    client.refreshConfig();
    assert(loaded == 0);
    assert(failures == 1);
    // A rejected read publishes the error without fabricating configuration.
    assert(!client.configError().isEmpty());
    assert(!client.configReady());

    int exports = 0;
    QObject::connect(&client, &TeslaClient::automagicExportFinished,
                     [&](bool ok, const QString &path, const QString &message) {
        assert(!ok);
        assert(path.isEmpty());
        assert(!message.isEmpty());
        assert(!client.automagicExportBusy());
        ++exports;
    });
    client.exportAutomagicFlows();
    assert(exports == 1); // Rejected admission must stop the UI busy state.

    acceptRequests = true;
    client.setConfig("", "", "phone", 20, 5, "flash-lights", "honk");
    assert(submitted.value("op").toString() == "set_config");
    assert(submitted.value("cover_left_action").toString() == "flash-lights");
    assert(submitted.value("cover_right_action").toString() == "honk");
    client.setNotificationSettings(true, {"lock", "honk"});
    assert(submitted.value("op").toString() == "set_notification");
    assert(submitted.value("enabled").toBool());
    assert(submitted.value("actions").toArray() == QJsonArray({"lock", "honk"}));

    QString leftAction, rightAction;
    QObject::connect(&client, &TeslaClient::configLoaded,
                     [&](const QString &, const QString &, const QString &, int, int,
                         bool, const QString &, const QString &left, const QString &right) {
        leftAction = left;
        rightAction = right;
    });
    int vehicleNameChanges = 0, notificationChanges = 0;
    QObject::connect(&client, &TeslaClient::vehicleNameChanged, [&] { ++vehicleNameChanges; });
    QObject::connect(&client, &TeslaClient::notificationSettingsChanged,
                     [&] { ++notificationChanges; });
    const QByteArray notification = R"({"type":"config_loaded","cover_left_action":"flash-lights","cover_right_action":"honk","vehicle_name":"Eel","notification_enabled":true,"notification_actions":["honk","lock"]})";
    const bool delivered = QMetaObject::invokeMethod(&client, "deliverNotification",
        Qt::DirectConnection, Q_ARG(QByteArray, notification));
    assert(delivered);
    assert(loaded == 1);
    assert(leftAction == "flash-lights");
    assert(rightAction == "honk");

    // Configuration properties: published from the reply, error cleared.
    assert(client.configReady());
    assert(client.configError().isEmpty());
    assert(client.coverLeftAction() == "flash-lights");
    assert(client.vehicleName() == "Eel" && vehicleNameChanges == 1);
    assert(client.notificationEnabled() && notificationChanges == 1);
    assert(client.notificationActions() == QStringList({"honk", "lock"}));

    int vinChanges = 0, keyChanges = 0, pemChanges = 0;
    QObject::connect(&client, &TeslaClient::vinChanged, [&] { ++vinChanges; });
    QObject::connect(&client, &TeslaClient::hasKeyChanged, [&] { ++keyChanges; });
    QObject::connect(&client, &TeslaClient::publicKeyPemChanged, [&] { ++pemChanges; });
    auto deliver = [&](const QByteArray &json) {
        const bool ok = QMetaObject::invokeMethod(&client, "deliverNotification",
            Qt::DirectConnection, Q_ARG(QByteArray, json));
        assert(ok);
    };
    const QByteArray paired = R"({"type":"config_loaded","vin":"5YJ3E1EA0PF000000","has_key":true,"pem":"PEM"})";
    deliver(paired);
    assert(client.vin() == "5YJ3E1EA0PF000000");
    assert(client.hasKey());
    assert(client.publicKeyPem() == "PEM");
    assert(vinChanges == 1 && keyChanges == 1 && pemChanges == 1);
    // An identical configuration reply changes no property, so bound pages
    // (e.g. the dashboard's status refresh) do not react to it.
    deliver(paired);
    assert(vinChanges == 1 && keyChanges == 1 && pemChanges == 1);
    assert(loaded == 3); // configLoaded itself still reports every reply.

    // A deleted key must not leave a stale PEM on display.
    deliver(R"({"type":"config_loaded","vin":"5YJ3E1EA0PF000000","has_key":false,"pem":"PEM"})");
    assert(!client.hasKey());
    assert(client.publicKeyPem().isEmpty());
    assert(vinChanges == 1);

    // A later failed reload keeps the last good values.
    acceptRequests = false;
    client.refreshConfig();
    assert(client.configReady());
    assert(client.vin() == "5YJ3E1EA0PF000000");
    assert(!client.configError().isEmpty());
}
