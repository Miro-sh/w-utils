Name:           w-utils
Version:        0.3.0
Release:        1%{?dist}
Summary:        Unix command-line tools rewritten in Rust

License:        MIT
URL:            https://github.com/Miro-sh/w-utils
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust

%description
w-utils is a collection of Unix command-line tools rewritten in Rust:
wcp, a drop-in replacement for cp, and wmv, a drop-in replacement for
mv, both with a live progress bar, ETA, and crash-safe atomic copies.

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
for tool in wcp wmv; do
    install -Dm755 target/release/$tool %{buildroot}%{_bindir}/$tool
    target/release/$tool --generate-man | gzip -9 > $tool.1.gz
    install -Dm644 $tool.1.gz %{buildroot}%{_mandir}/man1/$tool.1.gz
    target/release/$tool --generate-completions bash > $tool.bash
    install -Dm644 $tool.bash %{buildroot}%{_datadir}/bash-completion/completions/$tool
    target/release/$tool --generate-completions zsh > _$tool
    install -Dm644 _$tool %{buildroot}%{_datadir}/zsh/site-functions/_$tool
    target/release/$tool --generate-completions fish > $tool.fish
    install -Dm644 $tool.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/$tool.fish
done
install -Dm644 README.md %{buildroot}%{_docdir}/w-utils/README

%files
%{_bindir}/wcp
%{_bindir}/wmv
%{_mandir}/man1/wcp.1.gz
%{_mandir}/man1/wmv.1.gz
%{_datadir}/bash-completion/completions/wcp
%{_datadir}/bash-completion/completions/wmv
%{_datadir}/zsh/site-functions/_wcp
%{_datadir}/zsh/site-functions/_wmv
%{_datadir}/fish/vendor_completions.d/wcp.fish
%{_datadir}/fish/vendor_completions.d/wmv.fish
%doc %{_docdir}/w-utils/README
%license LICENSE

%changelog
* Thu Jul 23 2026 Miro-sh
- 0.3.0-1
- Add wmv (mv replacement), shell completions for both tools

* Tue Jul 22 2026 Miro-sh
- 0.1.7-1
- Initial COPR package
