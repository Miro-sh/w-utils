Name:           w-utils
Version:        0.1.7
Release:        1%{?dist}
Summary:        Unix command-line tools rewritten in Rust

License:        MIT
URL:            https://github.com/Miro-sh/w-utils
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust

%description
w-utils is a collection of Unix command-line tools rewritten in Rust.
Its first member is wcp, a drop-in replacement for cp with a live
progress bar, ETA, and crash-safe atomic copies.

%prep
%autosetup -n %{name}-%{version}
# Le repo épingle la cible musl ; sur COPR on compile en natif (glibc).
rm -f .cargo/config.toml
# Dépendances vendorisées : les builders COPR n'ont pas accès au réseau.
mkdir -p .cargo
cat > .cargo/config.toml <<'VENDOR'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
VENDOR

%build
cargo build --release --offline

%install
install -Dm755 target/release/wcp %{buildroot}%{_bindir}/wcp
target/release/wcp --generate-man | gzip -9 > wcp.1.gz
install -Dm644 wcp.1.gz %{buildroot}%{_mandir}/man1/wcp.1.gz
install -Dm644 README.md %{buildroot}%{_docdir}/w-utils/README

%files
%{_bindir}/wcp
%{_mandir}/man1/wcp.1.gz
%doc %{_docdir}/w-utils/README
%license LICENSE

%changelog
* Tue Jul 22 2026 Miro-sh
- 0.1.7-1
- Initial COPR package
