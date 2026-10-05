# Test fixture: a package with a %post scriptlet, which udnf must refuse.
Name:           udnftest-scriptlet
Version:        1.0
Release:        1
Summary:        udnf test package with a scriptlet
License:        MIT
BuildArch:      noarch
Requires:       libudnftest

%description
Package used by the udnf integration tests: it must be refused.

%install
install -Dm644 /dev/null %{buildroot}%{_datadir}/udnftest-scriptlet/marker

%post
echo "this must never run"

%files
%{_datadir}/udnftest-scriptlet/marker
