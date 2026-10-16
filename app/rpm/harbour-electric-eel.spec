Name:       harbour-electric-eel
Summary:    Control your Tesla over Bluetooth
Version:    0.3.8
Release:    1
License:    ASL 2.0
URL:        https://github.com/nappa85/ElectricEel
Source0:    %{name}-%{version}.tar.bz2
Requires:   sailfishsilica-qt5
Requires:   libkeepalive
BuildRequires:  pkgconfig(sailfishapp) >= 1.0.2
BuildRequires:  pkgconfig(Qt5Core)
BuildRequires:  pkgconfig(Qt5Qml)
BuildRequires:  pkgconfig(Qt5Quick)
BuildRequires:  desktop-file-utils

# Private library must not advertise a system-wide RPM capability.
%define __provides_exclude_from ^%{_datadir}/%{name}/lib/.*$
%define __requires_exclude ^libteslable\.so.*$

%description
Sandboxed Silica UI for controlling a Tesla over Bluetooth, grouped like
the official Tesla app: quick actions, climate, charging, locks & security,
trunk/frunk/windows, media, software, keys and diagnostics. BLE transport
runs through a cooperative org.bluez backend (no raw HCI takeover), driven
by an in-process Rust control core (staticlib) using a private Go protocol
library through an asynchronous C ABI. No privileged helper service,
no CAP_NET_ADMIN, no devel-su - a single self-contained Harbour app.

%prep
%setup -q -n %{name}-%{version}

%build
%qmake5
make %{?_smp_mflags}

%install
rm -rf %{buildroot}
%qmake5_install

chmod 0644 %{buildroot}%{_datadir}/%{name}/lib/libteslable.so
strip --strip-unneeded %{buildroot}%{_bindir}/%{name}
strip --strip-unneeded %{buildroot}%{_datadir}/%{name}/lib/libteslable.so

desktop-file-install --delete-original \
  --dir %{buildroot}%{_datadir}/applications \
   %{buildroot}%{_datadir}/applications/*.desktop

%files
%defattr(-,root,root,-)
%{_bindir}/%{name}
%{_datadir}/%{name}
%{_datadir}/applications/%{name}.desktop
%{_datadir}/icons/hicolor/*/apps/%{name}.png

%changelog
* Fri Oct 16 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.8-1
- Read the car's name over Bluetooth, show it instead of the VIN and allow renaming it from Settings
- Optional silent status notification with configurable quick actions
- Cover defaults: front trunk left, rear trunk right
- Open the dashboard scrolled to the top
- Log command failures to the phone-key log
* Fri Oct 16 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.7-1
- Fix phone-key proximity detection missing typical car approaches
- Log presence events and near RSSI to the phone-key log
* Thu Oct 15 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.6-1
- Code cleanup
* Thu Oct 15 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.5-1
- Fix dashboard freshness, pairing key validation and command parsing regressions
- Improve session concurrency and RX backpressure handling
- Complete translations in all 39 languages
* Wed Oct 14 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.4-1
- Fix charging indicators, navigation share races and pairing request handling
- Report configuration load failures and validate command arguments
- Clear failed FFI outputs and cap untouched Automagic exports
* Tue Oct 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.3-1
- Export Automagic presets through Rust using the Documents permission
- Move delayed-away policy and typed runtime notifications into Rust
- Validate Harbour packaging in CI and before release publication
* Tue Oct 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.2-1
- Fix phone-key recovery after protocol session teardown
- Settings button writes Automagic triggers and hotspot flows for PhoneKeyEvent
  (Pauli Kettunen)
- Hotspot off waits 3 minutes unless presence_inside arrives first
  (Pauli Kettunen)
* Tue Oct 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.1-1
- Use Go Tesla Command via FFI
* Sun Sep 27 2026 Marco Napetti <marco.napetti@proton.me> - 0.3.0-1
- CPU keepalive and move responsabilities to Rust core
* Sun Sep 27 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.12-1
- Backgroud process and logging improvements
* Sun Sep 20 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.11-1
- Share target registration: X-Share-Methods belongs in [Desktop Entry]
- Generic map-URL parsing (no per-service list)
- Parent/child transport over Unix socket with versioned handshake
* Sun Sep 20 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.10-1
- Share destination to car navigation over BLE (field-53 coordinates, field-21 address)
* Wed Aug 26 2026 Pauli Kettunen <pauligrinder@gmail.com> - 0.2.9-1
- Stabilize phone-key GATT reconnect and VCSEC presence
- Daily phone-key logs under Documents/ElectricEel
- Cover connection status and lock/trunk/frunk actions
* Sat Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.8-1
- Restart session on wakeup
* Sat Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.7-1
- Enable BlueZ AutoConnect
* Sat Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.6-1
- Ensure child process is killed with parent
* Tue Aug 18 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.5-1
- Phone as key
* Tue Aug 18 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.4-1
- Fix sliders
* Sat Aug 15 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.3-1
- Busy indicator in config pages
* Fri Aug 14 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.2-1
- Keyless drive
* Thu Aug 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.1-1
- Proximity unlock
* Thu Aug 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.0-1
- Renamed the app from harbour-teslacontrol to harbour-electric-eel (ElectricEel)
- BlueZ D-Bus BLE backend; in-process Rust core, no privileged helper service
* Wed Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.7-1
- Avoid config edit before config load
* Wed Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.6-1
- Car model
* Wed Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.5-1
- UI improvements
* Tue Aug 11 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.4-1
- Version bump
* Tue Aug 11 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.3-1
- UI revamped, thanks to @cypherpunks
* Tue Aug 11 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.2-1
- Bump version
* Tue Aug 11 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.1-1
- Rewrite helper in Rust
* Fri Aug 07 2026 Marco Napetti <marco.napetti@firma.ai> - 0.1.0-1
- Initial packaging.
