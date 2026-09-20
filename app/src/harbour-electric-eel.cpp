#include <sailfishapp.h>
#include <QGuiApplication>
#include <QQuickView>
#include <QLocale>
#include <QStringList>
#include <QTranslator>
#include <QtQml>

#include "teslaclient.h"

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

int main(int argc, char *argv[])
{
    QGuiApplication *app = SailfishApp::application(argc, argv);
    installAppTranslator(app);
    QQuickView *view = SailfishApp::createView();

    qmlRegisterType<TeslaClient>("harbour.electriceel", 1, 0, "TeslaClient");

    view->setSource(SailfishApp::pathTo(QStringLiteral("qml/harbour-electric-eel.qml")));
    view->show();

    return app->exec();
}
