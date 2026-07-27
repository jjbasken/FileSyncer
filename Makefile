# FileSyncer install/uninstall.
#
# Usage:
#   make                 build release binaries
#   make install         install into $(PREFIX)  (default /usr/local)
#   sudo make install    system-wide install
#   make install PREFIX=$HOME/.local   per-user install
#   make uninstall
#   make DESTDIR=/tmp/stage install     stage into a packaging root
#
# Honours the GNU conventions: PREFIX, DESTDIR, and the standard *dir splits.

CARGO   ?= cargo
PREFIX  ?= /usr/local
DESTDIR ?=

bindir      = $(PREFIX)/bin
datarootdir = $(PREFIX)/share
appsdir     = $(datarootdir)/applications
icondir     = $(datarootdir)/icons/hicolor/scalable/apps
metainfodir = $(datarootdir)/metainfo

APP_ID   = net.jjbasken.FileSyncer
RELEASE  = target/release

INSTALL         = install
INSTALL_PROGRAM = $(INSTALL) -Dm755
INSTALL_DATA    = $(INSTALL) -Dm644

.PHONY: all build release check test install uninstall clean

all: build

build release:
	$(CARGO) build --release --workspace

check test:
	$(CARGO) test --workspace

install: release
	$(INSTALL_PROGRAM) $(RELEASE)/filesyncer     $(DESTDIR)$(bindir)/filesyncer
	$(INSTALL_PROGRAM) $(RELEASE)/filesyncer-gui  $(DESTDIR)$(bindir)/filesyncer-gui
	$(INSTALL_DATA) data/$(APP_ID).desktop        $(DESTDIR)$(appsdir)/$(APP_ID).desktop
	$(INSTALL_DATA) data/icons/$(APP_ID).svg      $(DESTDIR)$(icondir)/$(APP_ID).svg
	$(INSTALL_DATA) data/$(APP_ID).metainfo.xml   $(DESTDIR)$(metainfodir)/$(APP_ID).metainfo.xml
	@echo
	@echo "Installed filesyncer and filesyncer-gui to $(DESTDIR)$(bindir)."
	@if [ -z "$(DESTDIR)" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then \
		gtk-update-icon-cache -qtf $(datarootdir)/icons/hicolor 2>/dev/null || true; \
	fi

uninstall:
	rm -f $(DESTDIR)$(bindir)/filesyncer
	rm -f $(DESTDIR)$(bindir)/filesyncer-gui
	rm -f $(DESTDIR)$(appsdir)/$(APP_ID).desktop
	rm -f $(DESTDIR)$(icondir)/$(APP_ID).svg
	rm -f $(DESTDIR)$(metainfodir)/$(APP_ID).metainfo.xml
	@echo "Removed FileSyncer from $(DESTDIR)$(PREFIX)."

clean:
	$(CARGO) clean
