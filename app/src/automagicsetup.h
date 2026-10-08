#ifndef AUTOMAGICSETUP_H
#define AUTOMAGICSETUP_H

#include <QObject>
#include <QString>

// Merges ElectricEel D-Bus triggers, hotspot actions, and flows into
// harbour-automagic's JSON, then asks automagicd to reload. Does not
// overwrite unrelated items. SSID/passphrase are never written or logged.
// Triggers listen for org.electriceel.PhoneKey1.PhoneKeyEvent.
class AutomagicSetup : public QObject
{
    Q_OBJECT

public:
    explicit AutomagicSetup(QObject *parent = nullptr);

    bool install(QString *message);

private:
    QString configDir() const;
    bool upsertFile(const QString &fileName, const QList<QVariantMap> &items,
                    QString *error);
    bool reloadDaemon(const QString &secret, QString *error);
};

#endif // AUTOMAGICSETUP_H
