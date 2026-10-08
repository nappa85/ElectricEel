#include "automagicsetup.h"

#include <QDebug>
#include <QDir>
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalSocket>
#include <QSaveFile>
#include <QVariantMap>

namespace {

const char *kDest = "org.electriceel.harbour-electric-eel";
const char *kPath = "/org/electriceel/PhoneKey";
const char *kIface = "org.electriceel.PhoneKey1";

void note(const QString &message)
{
    qWarning().noquote() << "automagic:" << message;
}

QVariantMap copyTransform(const char *in, const char *out)
{
    QVariantMap copy;
    copy.insert(QStringLiteral("type"), QStringLiteral("copy"));
    copy.insert(QStringLiteral("in"), QLatin1String(in));
    copy.insert(QStringLiteral("out"), QLatin1String(out));
    return copy;
}

// kind empty: fire on every PhoneKeyEvent. Otherwise filter arg0.
QVariantMap dbusSource(const QString &id, const QString &name, const QString &kind)
{
    QVariantMap s;
    s.insert(QStringLiteral("id"), id);
    s.insert(QStringLiteral("name"), name);
    s.insert(QStringLiteral("type"), QStringLiteral("dbus"));
    s.insert(QStringLiteral("enabled"), true);
    s.insert(QStringLiteral("trigger"), true);
    s.insert(QStringLiteral("address"), QStringLiteral("session"));
    s.insert(QStringLiteral("destination"), QLatin1String(kDest));
    s.insert(QStringLiteral("path"), QLatin1String(kPath));
    s.insert(QStringLiteral("interface"), QLatin1String(kIface));
    s.insert(QStringLiteral("signal"), QStringLiteral("PhoneKeyEvent"));
    if (!kind.isEmpty()) {
        QVariantMap filters;
        filters.insert(QStringLiteral("arg0"), kind);
        s.insert(QStringLiteral("filters"), filters);
    }
    s.insert(QStringLiteral("transformations"), QVariantList{
        copyTransform("arg0", "kind"),
        copyTransform("arg1", "vin"),
        copyTransform("arg2", "event_time"),
        copyTransform("arg3", "error"),
    });
    return s;
}

// ConnMan Technology.SetProperty(string, variant). automagicd is root and
// does not drop privileges for dbus_method, so this can toggle tethering.
// A shell `connmanctl` action runs as the session user, gets Permission
// denied, and still exits 0.
QVariantMap dbusTetherAction(const QString &id, const QString &name, bool on)
{
    QVariantMap nameArg;
    nameArg.insert(QStringLiteral("type"), QStringLiteral("s"));
    nameArg.insert(QStringLiteral("value"), QStringLiteral("Tethering"));
    QVariantMap valueArg;
    valueArg.insert(QStringLiteral("type"), QStringLiteral("v:b"));
    valueArg.insert(QStringLiteral("value"), on);

    QVariantMap a;
    a.insert(QStringLiteral("id"), id);
    a.insert(QStringLiteral("name"), name);
    a.insert(QStringLiteral("type"), QStringLiteral("dbus_method"));
    a.insert(QStringLiteral("enabled"), true);
    a.insert(QStringLiteral("address"), QStringLiteral("system"));
    a.insert(QStringLiteral("destination"), QStringLiteral("net.connman"));
    a.insert(QStringLiteral("path"), QStringLiteral("/net/connman/technology/wifi"));
    a.insert(QStringLiteral("interface"), QStringLiteral("net.connman.Technology"));
    a.insert(QStringLiteral("method"), QStringLiteral("SetProperty"));
    a.insert(QStringLiteral("timeout"), QStringLiteral("5s"));
    a.insert(QStringLiteral("args"), QVariantList{ nameArg, valueArg });
    return a;
}

// Shared Automagic state. A pending away run stores its _run_id here.
// presence_inside clears it, so the wait that is already asleep does not
// turn the hotspot off. A second presence_far leaves the token alone, so
// the first 3m timer is not restarted.
const char *kAwayState = "eel_hotspot_away";

QVariantMap actionStep(const QString &id, const QString &actionId,
                       const QVariantList &conditions = QVariantList())
{
    QVariantMap step;
    step.insert(QStringLiteral("id"), id);
    step.insert(QStringLiteral("type"), QStringLiteral("action"));
    step.insert(QStringLiteral("action"), actionId);
    step.insert(QStringLiteral("goto_alt"), QStringLiteral("end"));
    if (!conditions.isEmpty())
        step.insert(QStringLiteral("if"), conditions);
    return step;
}

QVariantMap stateStep(const QString &id, const QVariant &value, bool fromTemplate)
{
    QVariantMap params;
    params.insert(QStringLiteral("name"), QLatin1String(kAwayState));
    params.insert(fromTemplate ? QStringLiteral("template") : QStringLiteral("static"), value);

    QVariantMap step;
    step.insert(QStringLiteral("id"), id);
    step.insert(QStringLiteral("type"), QStringLiteral("action"));
    step.insert(QStringLiteral("function"), QStringLiteral("set_state"));
    step.insert(QStringLiteral("params"), params);
    step.insert(QStringLiteral("goto_alt"), QStringLiteral("end"));
    return step;
}

QVariantMap readAwayState(const QString &id, const QString &variable)
{
    QVariantMap params;
    params.insert(QStringLiteral("name"), QLatin1String(kAwayState));
    QVariantMap mapping;
    mapping.insert(QStringLiteral("state"), variable);

    QVariantMap step;
    step.insert(QStringLiteral("id"), id);
    step.insert(QStringLiteral("type"), QStringLiteral("get"));
    step.insert(QStringLiteral("function"), QStringLiteral("state"));
    step.insert(QStringLiteral("params"), params);
    step.insert(QStringLiteral("mapping"), mapping);
    step.insert(QStringLiteral("goto_alt"), QStringLiteral("end"));
    return step;
}

// End the flow when a 3m wait is already armed. A missing or empty state
// falls through (goto_alt is omitted).
QVariantMap skipIfAwayPending(const QString &id)
{
    QVariantMap exists;
    exists.insert(QStringLiteral("op"), QStringLiteral("exists"));
    exists.insert(QStringLiteral("left_var"), QStringLiteral("pending"));

    QVariantMap nonempty;
    nonempty.insert(QStringLiteral("logic"), QStringLiteral("and"));
    nonempty.insert(QStringLiteral("op"), QStringLiteral("!="));
    nonempty.insert(QStringLiteral("left_var"), QStringLiteral("pending"));
    nonempty.insert(QStringLiteral("right_const"), QStringLiteral(""));

    QVariantMap step;
    step.insert(QStringLiteral("id"), id);
    step.insert(QStringLiteral("type"), QStringLiteral("branch"));
    step.insert(QStringLiteral("goto"), QStringLiteral("end"));
    step.insert(QStringLiteral("if"), QVariantList{ exists, nonempty });
    return step;
}

QVariantMap waitStep(const QString &id, const QString &duration)
{
    QVariantMap params;
    params.insert(QStringLiteral("duration"), duration);

    QVariantMap step;
    step.insert(QStringLiteral("id"), id);
    step.insert(QStringLiteral("type"), QStringLiteral("wait"));
    step.insert(QStringLiteral("params"), params);
    step.insert(QStringLiteral("goto_alt"), QStringLiteral("end"));
    return step;
}

QVariantList stillThisAwayRun()
{
    QVariantMap same;
    same.insert(QStringLiteral("op"), QStringLiteral("=="));
    same.insert(QStringLiteral("left_var"), QStringLiteral("away_now"));
    same.insert(QStringLiteral("right_var"), QStringLiteral("_run_id"));
    return QVariantList{ same };
}

QVariantMap flow(const QString &id, const QString &name,
                 const QString &trigger, const QVariantList &steps)
{
    QVariantMap f;
    f.insert(QStringLiteral("id"), id);
    f.insert(QStringLiteral("name"), name);
    f.insert(QStringLiteral("enabled"), true);
    f.insert(QStringLiteral("triggers"), QStringList{ trigger });
    f.insert(QStringLiteral("steps"), steps);
    return f;
}

QJsonArray upsertAll(QJsonArray data, const QList<QVariantMap> &items)
{
    for (const QVariantMap &item : items) {
        const QJsonObject obj = QJsonObject::fromVariantMap(item);
        const QString id = obj.value(QStringLiteral("id")).toString();
        bool replaced = false;
        for (int i = 0; i < data.size(); ++i) {
            if (data.at(i).toObject().value(QStringLiteral("id")).toString() == id) {
                data.replace(i, obj);
                replaced = true;
                break;
            }
        }
        if (!replaced)
            data.append(obj);
    }
    return data;
}

} // namespace

AutomagicSetup::AutomagicSetup(QObject *parent)
    : QObject(parent)
{
}

QString AutomagicSetup::configDir() const
{
    const QString home = QDir::homePath() + QStringLiteral("/.config/app.qml/automagic");
    if (QDir(home).exists())
        return home;
    const QString fallback = QStringLiteral("/home/defaultuser/.config/app.qml/automagic");
    if (QDir(fallback).exists())
        return fallback;
    return home;
}

bool AutomagicSetup::upsertFile(const QString &fileName, const QList<QVariantMap> &items,
                                QString *error)
{
    const QString path = configDir() + QLatin1Char('/') + fileName;
    QJsonArray data;
    int version = 1;
    QFile in(path);
    if (in.exists()) {
        if (!in.open(QIODevice::ReadOnly)) {
            if (error)
                *error = QStringLiteral("cannot read %1").arg(fileName);
            return false;
        }
        const QJsonDocument doc = QJsonDocument::fromJson(in.readAll());
        in.close();
        if (!doc.isObject()) {
            if (error)
                *error = QStringLiteral("%1 is not Automagic JSON").arg(fileName);
            return false;
        }
        const QJsonObject root = doc.object();
        version = root.value(QStringLiteral("version")).toInt(1);
        data = root.value(QStringLiteral("data")).toArray();
    }

    QJsonObject root;
    root.insert(QStringLiteral("version"), version);
    root.insert(QStringLiteral("data"), upsertAll(data, items));

    QSaveFile out(path);
    if (!out.open(QIODevice::WriteOnly)) {
        if (error)
            *error = QStringLiteral("cannot write %1").arg(fileName);
        return false;
    }
    out.write(QJsonDocument(root).toJson(QJsonDocument::Indented));
    if (!out.commit()) {
        if (error)
            *error = QStringLiteral("cannot save %1").arg(fileName);
        return false;
    }
    return true;
}

bool AutomagicSetup::reloadDaemon(const QString &secret, QString *error)
{
    QLocalSocket sock;
    sock.connectToServer(QStringLiteral("/run/automagicd/automagicd.sock"));
    if (!sock.waitForConnected(1500)) {
        if (error)
            *error = QStringLiteral("automagicd not reachable");
        return false;
    }

    auto sendLine = [&](const QJsonObject &obj) -> bool {
        sock.write(QJsonDocument(obj).toJson(QJsonDocument::Compact));
        sock.write("\n");
        return sock.waitForBytesWritten(1500);
    };
    auto readLine = [&]() -> QJsonObject {
        if (!sock.waitForReadyRead(2000))
            return QJsonObject();
        QByteArray line;
        while (!line.contains('\n') && sock.waitForReadyRead(500))
            line += sock.readAll();
        const int nl = line.indexOf('\n');
        if (nl >= 0)
            line = line.left(nl);
        return QJsonDocument::fromJson(line).object();
    };

    QJsonObject auth;
    auth.insert(QStringLiteral("secret"), secret);
    if (!sendLine(auth)) {
        if (error)
            *error = QStringLiteral("automagicd write failed");
        return false;
    }
    const QJsonObject authReply = readLine();
    if (!authReply.value(QStringLiteral("ok")).toBool()) {
        if (error)
            *error = QStringLiteral("automagicd auth failed");
        return false;
    }

    QJsonObject reload;
    reload.insert(QStringLiteral("cmd"), QStringLiteral("reload"));
    if (!sendLine(reload)) {
        if (error)
            *error = QStringLiteral("automagicd reload write failed");
        return false;
    }
    const QJsonObject reloadReply = readLine();
    if (!reloadReply.value(QStringLiteral("ok")).toBool()) {
        if (error)
            *error = QStringLiteral("automagicd reload failed");
        return false;
    }
    return true;
}

bool AutomagicSetup::install(QString *message)
{
    auto fail = [&](const QString &text) -> bool {
        if (message)
            *message = text;
        return false;
    };
    auto okMsg = [&](const QString &text) -> bool {
        if (message)
            *message = text;
        return true;
    };

    const QString dir = configDir();
    if (!QDir(dir).exists()) {
        note(QStringLiteral("config dir missing"));
        return fail(QStringLiteral("Automagic config not found. Open Automagic once, then try again."));
    }

    const QList<QVariantMap> sources{
        dbusSource(QStringLiteral("eel_inside"),
                   QStringLiteral("ElectricEel inside car"),
                   QStringLiteral("presence_inside")),
        dbusSource(QStringLiteral("eel_far"),
                   QStringLiteral("ElectricEel walked away"),
                   QStringLiteral("presence_far")),
        dbusSource(QStringLiteral("eel_near"),
                   QStringLiteral("ElectricEel connected"),
                   QStringLiteral("presence_near")),
        dbusSource(QStringLiteral("eel_auth_ok"),
                   QStringLiteral("ElectricEel authorized"),
                   QStringLiteral("presence_auth_ok")),
        dbusSource(QStringLiteral("eel_presence"),
                   QStringLiteral("ElectricEel presence"),
                   QString())
    };

    const QList<QVariantMap> actions{
        dbusTetherAction(QStringLiteral("eel_hotspot_on"),
                         QStringLiteral("ElectricEel hotspot on"), true),
        dbusTetherAction(QStringLiteral("eel_hotspot_off"),
                         QStringLiteral("ElectricEel hotspot off"), false)
    };

    const QList<QVariantMap> flows{
        flow(QStringLiteral("eel_flow_hotspot_on"),
             QStringLiteral("ElectricEel hotspot on"),
             QStringLiteral("eel_inside"),
             QVariantList{
                 // Clear first so a wait already in progress loses its token
                 // even if the hotspot-on call fails.
                 [&]() {
                     QVariantMap cancel = stateStep(QStringLiteral("eel_step_cancel_away"),
                                                    QStringLiteral(""), false);
                     cancel.insert(QStringLiteral("goto_alt"),
                                   QStringLiteral("eel_step_hotspot_on"));
                     return cancel;
                 }(),
                 actionStep(QStringLiteral("eel_step_hotspot_on"),
                            QStringLiteral("eel_hotspot_on"))
             }),
        flow(QStringLiteral("eel_flow_hotspot_off"),
             QStringLiteral("ElectricEel hotspot off"),
             QStringLiteral("eel_far"),
             QVariantList{
                 readAwayState(QStringLiteral("eel_step_read_pending"),
                               QStringLiteral("pending")),
                 skipIfAwayPending(QStringLiteral("eel_step_away_already")),
                 stateStep(QStringLiteral("eel_step_arm_away"),
                           QStringLiteral("{{_run_id}}"), true),
                 [&]() {
                     QVariantMap wait = waitStep(QStringLiteral("eel_step_wait_away"),
                                                 QStringLiteral("3m"));
                     wait.insert(QStringLiteral("goto_alt"),
                                 QStringLiteral("eel_step_clear_away"));
                     return wait;
                 }(),
                 [&]() {
                     QVariantMap read = readAwayState(QStringLiteral("eel_step_read_away"),
                                                      QStringLiteral("away_now"));
                     read.insert(QStringLiteral("goto_alt"),
                                 QStringLiteral("eel_step_clear_away"));
                     return read;
                 }(),
                 [&]() {
                     QVariantMap off = actionStep(QStringLiteral("eel_step_hotspot_off"),
                                                  QStringLiteral("eel_hotspot_off"),
                                                  stillThisAwayRun());
                     off.insert(QStringLiteral("goto_alt"),
                                QStringLiteral("eel_step_clear_away"));
                     return off;
                 }(),
                 [&]() {
                     // Only the run that armed this wait may clear it. An
                     // older wait must not wipe a token a later away stored.
                     QVariantMap clear = stateStep(QStringLiteral("eel_step_clear_away"),
                                                   QStringLiteral(""), false);
                     clear.insert(QStringLiteral("if"), stillThisAwayRun());
                     return clear;
                 }()
             })
    };

    QString error;
    if (!upsertFile(QStringLiteral("data_sources.json"), sources, &error)
            || !upsertFile(QStringLiteral("actions.json"), actions, &error)
            || !upsertFile(QStringLiteral("flows.json"), flows, &error)) {
        note(error);
        return fail(error);
    }

    QString secret;
    QFile secretFile(dir + QStringLiteral("/secret.json"));
    if (secretFile.open(QIODevice::ReadOnly)) {
        const QJsonObject root = QJsonDocument::fromJson(secretFile.readAll()).object();
        secret = root.value(QStringLiteral("data")).toObject()
                 .value(QStringLiteral("shared_secret")).toString();
    }
    if (secret.isEmpty()) {
        note(QStringLiteral("wrote JSON, no daemon secret"));
        return okMsg(QStringLiteral("Wrote Automagic sources, actions, and flows. Open Automagic so the daemon reloads."));
    }

    if (!reloadDaemon(secret, &error)) {
        note(error);
        return okMsg(QStringLiteral("Wrote Automagic flows, but %1. Open Automagic to reload.").arg(error));
    }

    note(QStringLiteral("installed sources actions flows"));
    return okMsg(QStringLiteral("Added ElectricEel triggers and hotspot flows to Automagic."));
}
