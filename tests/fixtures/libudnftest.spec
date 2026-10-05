# Test fixture: a shared library not present on the system.
%global debug_package %{nil}
Name:           libudnftest
Version:        %{?ver}%{!?ver:1.0}
Release:        1
Summary:        udnf test library
License:        MIT
BuildRequires:  gcc

%description
Shared library used by the udnf integration tests.

%prep
cat > udnftest.c <<'SRC'
int udnftest_answer(void) { return %{lua: print((rpm.expand("%{version}"):gsub("%.", "")))}; }
SRC

%build
gcc -shared -fPIC -Wl,-soname,libudnftest.so.1 -o libudnftest.so.1 udnftest.c

%install
install -Dm755 libudnftest.so.1 %{buildroot}%{_libdir}/libudnftest.so.1
# Hardlink set whose content changes with the version (3 members: the payload carries the content
# with one of them only, the others must take it from the member written in the same run).
d=%{buildroot}%{_datadir}/udnftest
mkdir -p $d
echo %{version} > $d/a
ln $d/a $d/b
ln $d/a $d/c
# compat/ is a directory in 1.0 and becomes a symlink in 2.0 (what %%pretrans migrates in Fedora).
%if "%{version}" == "1.0"
mkdir -p $d/compat
echo old > $d/compat/data
%else
mkdir -p $d/new
echo new > $d/new/data
ln -s new $d/compat
%endif

%files
%{_libdir}/libudnftest.so.1
%{_datadir}/udnftest
