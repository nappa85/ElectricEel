#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusPendingCall>
#include <QDBusPendingReply>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QSignalSpy>
#include <QtTest>

class PhoneKeyEventsTest : public QObject
{
    Q_OBJECT
signals:
    void received(const QString &kind, const QString &vin,
                   const QString &time, const QString &errorMessage);
    void stateReceived(bool active, const QString &link, const QString &status);
private slots:
    void onState(bool active, const QString &link, const QString &status) {
        emit stateReceived(active, link, status);
    }
    void onEvent(const QString &kind, const QString &vin,
                 const QString &time, const QString &errorMessage)
    {
        emit received(kind, vin, time, errorMessage);
    }

    void publishesEveryEventWithIntrospection()
    {
        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.isConnected());
        const QString service = QStringLiteral("org.electriceel.harbour-electric-eel");
        const QString path = QStringLiteral("/org/electriceel/PhoneKey");
        const QString iface = QStringLiteral("org.electriceel.PhoneKey1");
        QVERIFY(bus.registerService(service));
        QVERIFY(bus.connect(service, path, iface, QStringLiteral("PhoneKeyEvent"), this,
                            SLOT(onEvent(QString,QString,QString,QString))));
        QSignalSpy spy(this, SIGNAL(received(QString,QString,QString,QString)));
        QVERIFY(bus.connect(service, path, iface, QStringLiteral("PhoneKeyStateChanged"), this,
                            SLOT(onState(bool,QString,QString))));
        QSignalSpy states(this, SIGNAL(stateReceived(bool,QString,QString)));
        QVERIFY(spy.isValid());

        QQmlEngine engine;
        QQmlComponent source(&engine);
        source.setData("import QtQml 2.2\nQtObject {\n"
                       "signal phoneKeyEvent(string kind, string vin, string time, string errorMessage)\n"
                       "signal phoneKeyStateChanged(bool active, string link, string status)\n"
                       "}\n", QUrl());
        QScopedPointer<QObject> client(source.create());
        QVERIFY2(client, qPrintable(source.errorString()));
        const QString publisherPath = QFINDTESTDATA("../qml/PhoneKeyEvents.qml");
        QVERIFY(!publisherPath.isEmpty());
        QQmlComponent publisher(&engine, QUrl::fromLocalFile(publisherPath));
        QScopedPointer<QObject> object(publisher.create());
        QVERIFY2(object, qPrintable(publisher.errorString()));
        QVERIFY(object->setProperty("client", QVariant::fromValue(client.data())));

        auto introspection = bus.asyncCall(QDBusMessage::createMethodCall(
            service, path, QStringLiteral("org.freedesktop.DBus.Introspectable"),
            QStringLiteral("Introspect")));
        QTRY_VERIFY(introspection.isFinished());
        QDBusPendingReply<QString> reply(introspection);
        QVERIFY2(!reply.isError(), qPrintable(reply.error().message()));
        const QString xml = reply.value();
        QVERIFY(xml.contains(iface));
        QVERIFY(xml.contains(QStringLiteral("signal name=\"PhoneKeyEvent\"")));
        const QString signalXml = xml.mid(xml.indexOf(QStringLiteral("<signal name=\"PhoneKeyEvent\">")))
                                     .section(QStringLiteral("</signal>"), 0, 0);
        QCOMPARE(signalXml.count(QStringLiteral("type=\"s\"")), 4);
        const QString stateXml = xml.mid(xml.indexOf(QStringLiteral("<signal name=\"PhoneKeyStateChanged\">")))
                                     .section(QStringLiteral("</signal>"), 0, 0);
        QCOMPARE(stateXml.count(QStringLiteral("type=\"b\"")), 1);
        QCOMPARE(stateXml.count(QStringLiteral("type=\"s\"")), 2);
        QVERIFY(QMetaObject::invokeMethod(client.data(), "phoneKeyStateChanged",
                Q_ARG(bool, true), Q_ARG(QString, QStringLiteral("scanning")),
                Q_ARG(QString, QStringLiteral("arbitrary prose"))));
        QTRY_COMPARE(states.count(), 1);
        QCOMPARE(states.first().at(0).type(), QVariant::Bool);
        QCOMPARE(states.first().at(1).toString(), QStringLiteral("scanning"));
        QCOMPARE(states.first().at(2).toString(), QStringLiteral("arbitrary prose"));

        const QString vin = QStringLiteral("5YJ3E1EA0PF000000");
        const QString time = QStringLiteral("2026-10-07T12:00:00+02:00");
        for (const QString &kind : {QStringLiteral("presence_inside"),
                                   QStringLiteral("presence_auth_ok"),
                                   QStringLiteral("presence_auth_ok")}) {
            QVERIFY(QMetaObject::invokeMethod(client.data(), "phoneKeyEvent",
                    Q_ARG(QString, kind), Q_ARG(QString, vin), Q_ARG(QString, time),
                    Q_ARG(QString, QString())));
        }
        QTRY_COMPARE(spy.count(), 3);
        QCOMPARE(spy.at(0).at(0).toString(), QStringLiteral("presence_inside"));
        QCOMPARE(spy.at(1).at(0).toString(), QStringLiteral("presence_auth_ok"));
        QCOMPARE(spy.at(1), spy.at(2));
        for (const auto &event : spy) {
            QCOMPARE(event.size(), 4);
            QCOMPARE(event.at(1).toString(), vin);
            QCOMPARE(event.at(2).toString(), time);
            QVERIFY(event.at(3).toString().isEmpty());
            for (const auto &argument : event)
                QCOMPARE(argument.type(), QVariant::String);
        }
        const QString error = QStringLiteral("GATT link dropped");
        QVERIFY(QMetaObject::invokeMethod(client.data(), "phoneKeyEvent",
                Q_ARG(QString, QStringLiteral("presence_disconnected")), Q_ARG(QString, vin),
                Q_ARG(QString, time), Q_ARG(QString, error)));
        QTRY_COMPARE(spy.count(), 4);
        QCOMPARE(spy.at(3).at(3).toString(), error);
        QVERIFY(bus.unregisterService(service));
    }
};

QTEST_GUILESS_MAIN(PhoneKeyEventsTest)
#include "phonekeyevents_test.moc"
