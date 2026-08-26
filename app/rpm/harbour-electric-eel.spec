Name:       harbour-electric-eel
Summary:    Control your Tesla over Bluetooth
Version:    0.2.9
Release:    1
License:    ASL 2.0
URL:        https://github.com/nappa85/ElectricEel
Source0:    %{name}-%{version}.tar.bz2
Requires:   sailfishsilica-qt5 >= 0.10.9
Requires:   qt5-qtcore
Requires:   qt5-qtdeclarative
BuildRequires:  pkgconfig(sailfishapp) >= 1.0.2
BuildRequires:  pkgconfig(Qt5Core)
BuildRequires:  pkgconfig(Qt5Qml)
BuildRequires:  pkgconfig(Qt5Quick)
BuildRequires:  desktop-file-utils

%description
Sandboxed Silica UI for controlling a Tesla over Bluetooth, grouped like
the official Tesla app: quick actions, climate, charging, locks & security,
trunk/frunk/windows, media, software, keys and diagnostics. BLE transport
runs through a cooperative org.bluez backend (no raw HCI takeover), driven
by an in-process Rust control core (staticlib) that spawns the bundled
tesla-session binary. No privileged helper service, no CAP_NET_ADMIN, no
devel-su - a single self-contained Harbour app.

%prep
%setup -q -n %{name}-%{version}

%build
%qmake5
make %{?_smp_mflags}

%install
rm -rf %{buildroot}
%qmake5_install

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
* Wed Aug 26 2026 Pauli Kettunen <pauligrinder@gmail.com> - 0.2.9-1
- Stabilize phone-key GATT reconnect and VCSEC presence
- Daily phone-key logs under Documents/ElectricEel
- Cover connection status and lock/trunk/frunk actions
* Thu Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.8-1
- Restart session on wakeup
* Thu Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.7-1
- Enable BlueZ AutoConnect
* Thu Aug 22 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.6-1
- Ensure child process is killed with parent
* Thu Aug 18 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.5-1
- Phone as key
* Thu Aug 18 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.4-1
- Fix sliders
* Thu Aug 15 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.3-1
- Busy indicator in config pages
* Thu Aug 14 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.2-1
- Keyless drive
* Thu Aug 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.1-1
- Proximity unlock
* Thu Aug 13 2026 Marco Napetti <marco.napetti@proton.me> - 0.2.0-1
- Renamed the app from harbour-teslacontrol to harbour-electric-eel (ElectricEel)
- BlueZ D-Bus BLE backend; in-process Rust core, no privileged helper service
* Tue Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.7-1
- Avoid config edit before config load
* Tue Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.6-1
- Car model
* Tue Aug 12 2026 Marco Napetti <marco.napetti@proton.me> - 0.1.5-1
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
