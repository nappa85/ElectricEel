TARGET = harbour-electric-eel

CONFIG += sailfishapp

# Single source of the app version, surfaced on the Settings page and
# compared against the core's GetVersion. The release workflow stamps
# this (and helper/Cargo.toml) from the same git tag, so a matched pair
# reports equal versions. Keep in sync with helper/Cargo.toml when bumping
# outside a release.
VERSION = 0.2.11
DEFINES += APP_VERSION=\\\"$$VERSION\\\"

# In-process Rust control core (docs/architecture.md phase 4): the cbindgen
# header + aarch64 staticlib are cross-built on the host by
# helper/make-app-bundle.sh and staged into thirdparty/ here.
INCLUDEPATH += $$PWD/thirdparty
LIBS += $$PWD/thirdparty/libelectriceelcore.a -lpthread -ldl -lm

SOURCES += \
    src/harbour-electric-eel.cpp \
    src/teslaclient.cpp

HEADERS += \
    src/teslaclient.h

DISTFILES += \
    rpm/harbour-electric-eel.spec \
    harbour-electric-eel.desktop \
    qml/harbour-electric-eel.qml \
    qml/cover/CoverPage.qml \
    qml/pages/*.qml \
    qml/js/*.js \
    translations/harbour-electric-eel.ts \
    translations/harbour-electric-eel_it.ts \
    img/model3.png \
    img/models.png \
    img/modelx.png \
    img/modely.png \
    img/cybertruck.png \
    img/icons/*.svg

# The sailfishapp qmake feature auto-installs TARGET.desktop and the
# qml/ directory, but launcher icons and the in-app img/ assets used by
# the QML (car graphic + SVG command icons) need explicit INSTALLS rules
# - this is the standard boilerplate every Sailfish app template carries.
# img/ must land under /usr/share/$$TARGET (sibling of qml/) so the
# relative "../../img/..." paths FirstPage.qml / CommandCatalog.js use are
# preserved on-device.
imgdir.files = img
imgdir.path = /usr/share/$${TARGET}

# tesla-session child (Go) bundled by helper/make-app-bundle.sh; the in-process
# Rust core (~/.local/share data dir) spawns it from this read-only app dir.
# Binaries keep 0755 here; no setcap/CAP_NET_ADMIN needed with the bluez
# backend (Phase 4/5).
bindir.files = bin
bindir.path = /usr/share/$${TARGET}

icon86.files = icons/86x86/harbour-electric-eel.png
icon86.path = /usr/share/icons/hicolor/86x86/apps
icon108.files = icons/108x108/harbour-electric-eel.png
icon108.path = /usr/share/icons/hicolor/108x108/apps
icon128.files = icons/128x128/harbour-electric-eel.png
icon128.path = /usr/share/icons/hicolor/128x128/apps
icon172.files = icons/172x172/harbour-electric-eel.png
icon172.path = /usr/share/icons/hicolor/172x172/apps

# i18n (see tools/build-qm.sh + translations/): qsTr() catalogs compiled
# to .qm are installed beside qml/ and loaded per locale in main().
TRANSLATIONS += \
    translations/harbour-electric-eel_bg.ts \
    translations/harbour-electric-eel_bn.ts \
    translations/harbour-electric-eel_cs.ts \
    translations/harbour-electric-eel_da.ts \
    translations/harbour-electric-eel_de.ts \
    translations/harbour-electric-eel_el.ts \
    translations/harbour-electric-eel_es.ts \
    translations/harbour-electric-eel_et.ts \
    translations/harbour-electric-eel_fi.ts \
    translations/harbour-electric-eel_fr.ts \
    translations/harbour-electric-eel_gu.ts \
    translations/harbour-electric-eel_hi.ts \
    translations/harbour-electric-eel_hu.ts \
    translations/harbour-electric-eel_it.ts \
    translations/harbour-electric-eel_kn.ts \
    translations/harbour-electric-eel_lt.ts \
    translations/harbour-electric-eel_lv.ts \
    translations/harbour-electric-eel_ml.ts \
    translations/harbour-electric-eel_mr.ts \
    translations/harbour-electric-eel_nb.ts \
    translations/harbour-electric-eel_nl.ts \
    translations/harbour-electric-eel_pa.ts \
    translations/harbour-electric-eel_pl.ts \
    translations/harbour-electric-eel_pt.ts \
    translations/harbour-electric-eel_pt_BR.ts \
    translations/harbour-electric-eel_ro.ts \
    translations/harbour-electric-eel_ru.ts \
    translations/harbour-electric-eel_sk.ts \
    translations/harbour-electric-eel_sl.ts \
    translations/harbour-electric-eel_sv.ts \
    translations/harbour-electric-eel_ta.ts \
    translations/harbour-electric-eel_te.ts \
    translations/harbour-electric-eel_tr.ts \
    translations/harbour-electric-eel_tt.ts \
    translations/harbour-electric-eel_uk.ts \
    translations/harbour-electric-eel_vi.ts \
    translations/harbour-electric-eel_zh_CN.ts \
    translations/harbour-electric-eel_zh_HK.ts \
    translations/harbour-electric-eel_zh_TW.ts

transqm.files = translations/*.qm
transqm.path = /usr/share/$${TARGET}/translations

INSTALLS += imgdir bindir icon86 icon108 icon128 icon172 transqm
