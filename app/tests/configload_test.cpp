#include "../src/teslaclient.h"
#include <QGuiApplication>
#include <cassert>

extern "C" {
#include "runtime_stub/electriceelcore.h"

bool runtime_submit(Runtime *, const char *) { return false; }
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
}
