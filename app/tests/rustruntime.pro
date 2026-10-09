TEMPLATE = app
TARGET = rustruntime-test
QT = core gui testlib
CONFIG += console testcase c++11
DEFINES += APP_VERSION=\\\"test\\\"
INCLUDEPATH += ../thirdparty
LIBS += $$PWD/../thirdparty/libelectriceelcore.a -lpthread -ldl -lm
SOURCES += rustruntime_test.cpp ../src/teslaclient.cpp
HEADERS += ../src/teslaclient.h
