# Test fixture: a program linked against libudnftest, with desktop entry and absolute symlink.
%global debug_package %{nil}
Name:           udnftest-bin
Version:        %{?ver}%{!?ver:1.0}
Release:        1
Summary:        udnf test program
License:        MIT
BuildRequires:  gcc
Requires:       libudnftest >= %{version}

%description
Program used by the udnf integration tests.

%prep
cat > main.c <<'SRC'
#include <stdio.h>
int udnftest_answer(void);
int main(void) { printf("udnftest %d\n", udnftest_answer()); return 0; }
SRC

%build
printf 'int udnftest_answer(void){return 0;}' > stub.c
gcc -shared -fPIC -Wl,-soname,libudnftest.so.1 -o libudnftest.so stub.c
gcc -o udnftest main.c -L. -ludnftest

%install
install -Dm755 udnftest %{buildroot}%{_bindir}/udnftest
install -Dm755 /dev/stdin %{buildroot}%{_libexecdir}/udnftest/helper <<'SH'
#!/bin/sh
echo helper
SH
ln -s %{_libexecdir}/udnftest/helper %{buildroot}%{_bindir}/udnftest-helper
# Same layout as nodejs/npm: an interpreter in /usr/bin (like node-24), a script using it in
# its shebang under /usr/lib, exposed in /usr/bin by a relative symlink.
install -Dm755 /dev/stdin %{buildroot}%{_bindir}/udnftest-interp <<'SH'
#!/bin/sh
case "$LD_LIBRARY_PATH" in *udnf/root*) echo "interp env ${1##*/}" ;; *) echo "interp noenv" ;; esac
SH
install -Dm755 /dev/stdin %{buildroot}%{_prefix}/lib/udnftest/script.js <<'SH'
#!/usr/bin/udnftest-interp
SH
ln -s ../lib/udnftest/script.js %{buildroot}%{_bindir}/udnftest-script
install -Dm644 /dev/stdin %{buildroot}%{_datadir}/applications/udnftest.desktop <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=udnf test
Exec=/usr/bin/udnftest %U
Icon=udnftest
Actions=helper;

[Desktop Action helper]
Name=helper
Exec=/usr/libexec/udnftest/helper
DESKTOP
install -Dm644 /dev/stdin %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/udnftest.svg <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg"/>
SVG

%files
%{_bindir}/udnftest
%{_bindir}/udnftest-helper
%{_bindir}/udnftest-script
%{_bindir}/udnftest-interp
%{_prefix}/lib/udnftest
%{_libexecdir}/udnftest
%{_datadir}/applications/udnftest.desktop
%{_datadir}/icons/hicolor/scalable/apps/udnftest.svg
