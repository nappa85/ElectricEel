#include <sailfishapp.h>
#include <QGuiApplication>
#include <QQuickView>
#include <QLocale>
#include <QStringList>
#include <QTranslator>
#include <QtQml>
#include <QDir>
#include <QStandardPaths>
#include <memory>

#include "teslaclient.h"

extern "C" {
#include "electriceelcore.h"
}

int main(int argc, char *argv[])
{
    return electric_eel_app_main(argc, argv);
}

// Loads harbour-electric-eel_<lang>.qm for the system locale (full
// "it_IT" first, then bare "it") from the translations/ dir shipped in
// the RPM. Catalogs are built by tools/build-qm.sh from qsTr() strings;
// untranslated locales fall back to the English sources. See
// docs/translations.md (workflow).
static void installAppTranslator(QGuiApplication *app)
{
    const QString dir = SailfishApp::pathTo(QStringLiteral("translations")).toLocalFile();
    const QString locale = QLocale::system().name();
    QStringList candidates;
    candidates << locale;
    const int sep = locale.indexOf(QLatin1Char('_'));
    if (sep > 0)
        candidates << locale.left(sep);
    for (const QString &tag : candidates) {
        QTranslator *translator = new QTranslator(app);
        if (translator->load(QStringLiteral("harbour-electric-eel_%1").arg(tag), dir)) {
            app->installTranslator(translator);
            return;
        }
        delete translator;
    }
}

static QGuiApplication *application = nullptr;
static QByteArray stateDirectory;

// Called by Rust's executable entrypoint, before Core is constructed.
extern "C" const char *electric_eel_ui_prepare(int argc, char *argv[])
{
    // QGuiApplication retains argc by reference for its lifetime.
    static int qtArgc;
    qtArgc = argc;
    application = SailfishApp::application(qtArgc, argv);
    installAppTranslator(application);
    const QString state = QStandardPaths::writableLocation(QStandardPaths::AppDataLocation);
    const QString documents = QStandardPaths::writableLocation(QStandardPaths::DocumentsLocation);
    const QString logs = documents.isEmpty() ? state + "/logs" : documents + "/ElectricEel";
    if (!QDir().mkpath(state)) {
        qCritical() << "electric-eel: cannot create state dir" << state;
        return nullptr;
    }
    if (!QDir().mkpath(logs)) {
        qCritical() << "electric-eel: cannot create log dir" << logs;
        return nullptr;
    }
    qputenv("ELECTRIC_EEL_LOG_DIR", logs.toUtf8());
    stateDirectory = state.toUtf8();
    return stateDirectory.constData();
}

extern "C" int electric_eel_ui_run(Runtime *runtime)
{
    TeslaClient client(runtime);
    std::unique_ptr<QQuickView> view(SailfishApp::createView());
    view->rootContext()->setContextProperty(QStringLiteral("electricEelClient"), &client);

    view->setSource(SailfishApp::pathTo(QStringLiteral("qml/harbour-electric-eel.qml")));
    if (view->status() == QQuickView::Error) {
        qWarning() << view->errors();
        return 1;
    }
    view->show();

    return application->exec();
}

extern "C" void electric_eel_ui_cleanup()
{
    delete application;
    application = nullptr;
}
