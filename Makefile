# statusline: build, test, and install the menu bar tray as a login item.
#
#   make signing-cert  once per Mac: a self-signed identity to sign the tray with
#   make install       build, bundle, sign, register, and (re)load the LaunchAgent
#   make uninstall     unload the LaunchAgent and remove the app and the plist
#
# The tray is installed as an app bundle, not a bare binary: macOS only
# delivers native notifications to a signed bundle that LaunchServices knows
# about. Nothing here needs sudo. `install` writes the app bundle, the
# LaunchAgent plist and the log directory; `uninstall` removes the first two.
#
# The status line itself is a plain binary: `cargo install --path crates/statusline`.

LABEL      := com.jakub.statusline-tray
APP        := $(HOME)/Applications/Statusline Tray.app
EXE        := $(APP)/Contents/MacOS/statusline-tray
PLIST      := $(HOME)/Library/LaunchAgents/$(LABEL).plist
LOG        := $(HOME)/Library/Logs/statusline-tray.log
DOMAIN     := gui/$(shell id -u)
STAGE      := target/app-stage/Statusline Tray.app
LSREGISTER := /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
PACKAGING  := crates/sl-tray/packaging
# The identity `make signing-cert` creates. Signing with the same one every
# build keeps the tray's access to its GitHub token in the Keychain; ad-hoc
# signing (`-`, used when there is none) makes every build a new app to it.
CERT_NAME  := Statusline Dev
SIGN_ID    ?= $(shell security find-identity -p codesigning 2>/dev/null | grep -q '"$(CERT_NAME)"' && echo '$(CERT_NAME)' || echo -)

.PHONY: build test fmt lint run icon signing-cert install uninstall restart logs

build:
	cargo build --release -p sl-tray

test:
	cargo test --workspace

fmt:
	cargo fmt --all --check

lint: fmt
	cargo clippy --workspace --all-targets -- -D warnings

# Foreground, with tracing. Quit it from its menu (or ^C).
run: build
	SL_TRAY_DEBUG=1 target/release/statusline-tray

# Redraw the app icon (only needed after editing make-icon.swift; the result
# is committed).
icon:
	rm -rf target/AppIcon.iconset
	swift $(PACKAGING)/make-icon.swift target/AppIcon.iconset
	iconutil -c icns target/AppIcon.iconset -o $(PACKAGING)/AppIcon.icns

signing-cert:
	$(PACKAGING)/make-signing-cert.sh '$(CERT_NAME)'

install: build
	@[ '$(SIGN_ID)' != '-' ] || echo "warning: signing ad-hoc; run 'make signing-cert' once, or every build will have to be allowed the GitHub token again"
	# Assemble and sign a staged bundle: never rewrite a running signed binary
	# in place (macOS can SIGKILL it with "Code Signature Invalid").
	rm -rf "$(STAGE)"
	mkdir -p "$(STAGE)/Contents/MacOS"
	mkdir -p "$(STAGE)/Contents/Resources"
	cp $(PACKAGING)/Info.plist "$(STAGE)/Contents/Info.plist"
	cp $(PACKAGING)/AppIcon.icns "$(STAGE)/Contents/Resources/AppIcon.icns"
	cp target/release/statusline-tray "$(STAGE)/Contents/MacOS/statusline-tray"
	# Hardened runtime: no injected libraries or debugger, so nothing can
	# borrow the tray's signature to read its Keychain item. With
	# `make signing-cert`'s key this asks for your login password.
	codesign --force --options runtime --sign '$(SIGN_ID)' --identifier $(LABEL) "$(STAGE)"
	-launchctl bootout $(DOMAIN)/$(LABEL) 2>/dev/null; \
		for i in 1 2 3 4 5 6 7 8 9 10; do \
			launchctl print $(DOMAIN)/$(LABEL) >/dev/null 2>&1 || break; sleep 0.3; \
		done
	mkdir -p "$(HOME)/Applications" "$(HOME)/Library/LaunchAgents" "$(HOME)/Library/Logs"
	rm -rf "$(APP)"
	cp -R "$(STAGE)" "$(APP)"
	'$(LSREGISTER)' -f "$(APP)"
	sed -e 's|@EXE@|$(EXE)|g' -e 's|@LOG@|$(LOG)|g' -e 's|@LABEL@|$(LABEL)|g' \
		$(PACKAGING)/$(LABEL).plist.in > "$(PLIST)"
	plutil -lint "$(PLIST)"
	launchctl bootstrap $(DOMAIN) "$(PLIST)"
	@echo "installed $(APP) and $(PLIST)"

uninstall:
	-launchctl bootout $(DOMAIN)/$(LABEL)
	-'$(LSREGISTER)' -u "$(APP)"
	rm -rf "$(APP)"
	rm -f "$(PLIST)"

restart:
	launchctl kickstart -k $(DOMAIN)/$(LABEL)

logs:
	tail -f "$(LOG)"
