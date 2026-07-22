# COPR appelle "make -f .copr/Makefile srpm outdir=<dir>" dans le clone du repo.
# La version est dérivée du dernier tag git (v0.1.7 -> 0.1.7).
srpm:
	dnf -y install cargo rpm-build git
	git fetch --tags --force 2>/dev/null || true
	VERSION=$$(git describe --tags --abbrev=0 | sed 's/^v//'); \
	sed -i "s/^Version:.*/Version: $$VERSION/" packaging/w-utils.spec; \
	cargo vendor vendor; \
	mkdir w-utils-$$VERSION; \
	git archive --format=tar HEAD | tar -x -C w-utils-$$VERSION; \
	cp -r vendor w-utils-$$VERSION/; \
	tar czf $(outdir)/w-utils-$$VERSION.tar.gz w-utils-$$VERSION; \
	rpmbuild -bs --define "_sourcedir $(outdir)" --define "_srcrpmdir $(outdir)" packaging/w-utils.spec
