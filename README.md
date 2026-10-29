# Syphon app (Windows and Linux)

The Syphon app opens straight into your Syphon library. You don't need to type an address or use a browser. It works at home, over your own network, and away from home, through the VPN box at your home, with WireGuard built into the app. The app no longer uses Tor. It also has a **TV mode** that turns a spare computer or mini PC plugged into a TV into a dedicated Syphon player.

## Using it

- **First start at home:** choose "I'm at home". The app looks for your Syphon on the network and lists what it finds. Pick yours, or type its address (for example `192.168.1.20`), then sign in once.
- **First start away from home:** choose "I'm away from home" and paste a **VPN code** from the owner. It starts with `SYPHON-VPN-1:` and comes from the VPN box (`sudo syphon-vpn add "Mia's laptop"`). The app connects to the box, makes a key of its own (which never leaves the computer) and puts it in place of the code's one-time key. The code then can't be used again. The first sign-in from outside needs one extra step, as in the browser: someone at home approves it, or you type your two-step code. After that the computer is trusted. Leave "Remember me on this computer" on, and from then on the app opens straight into your library.
- **Who's watching?** Several people can sign in on one computer. Each one keeps their own sign-in, progress and settings, and switching takes one click: avatar menu > **Switch profile**. To show the picker every time the app opens, turn on **Ask "Who's watching?"** in App settings.
- **Away from home:** the app tries your home network first. When the node isn't there, it connects through the VPN box.
  - **When it can't connect:** App settings > Away from home > **Test the VPN connection** goes through it step by step (your home's address found? the box answers? Syphon answers through it?) and says what to check at the step that fails. The same test is on the "Cannot reach" screen.
  - **Built-in WireGuard:** it runs inside the app. No WireGuard app, network adapter, driver or administrator rights are needed, and nothing else on the computer goes through it: only the app's own connection to your Syphon.
  - A computer set up at home gets its VPN code under App settings > Away from home.
  - While away, the app checks every couple of minutes whether you're home again and switches to the direct way.
- **A sleeping Syphon** (Syphon: Settings > Device > Sleep):
  - **At home** the app wakes it itself: a wake-on-LAN packet to its network card, then it waits up to a minute and a bit ("Waking Living Room…"). The app learns the card's address from Syphon when someone signs in at home.
  - **Away from home** the VPN box wakes it as soon as the app connects, and the app waits for it the same way.
  - A browser doesn't wake it.
- **TV mode** (App settings > TV mode):
  - The app runs full screen with no window frame and uses the big-screen layout.
  - It shows "Who's watching?" every time it starts.
  - **Start with this computer** makes a mini PC behind the TV a Syphon box: switch it on and Syphon is there.
  - **Controls:** arrows to move, OK/Enter to choose, Back/Esc to go back. Hold Back for the menu (switch profile, settings). In the player, OK pauses, ◀ ▶ jump 10 seconds, and holding them goes further.
  - **What works as a remote:** a keyboard, a USB or Bluetooth TV remote that acts as a keyboard, an air mouse, a game controller (D-pad or stick, A = OK, B = Back, Start = play/pause) and media keys.
  - **PIN:** with one set, leaving TV mode, quitting and the settings all need it.
  - The screen stays on while something plays.

The TV layout lives on the node at `#/tv`, so a TV with a web browser can open it too, and you can try it from the avatar menu > **TV view**.

## Privacy

- **Where it connects:** the app talks only to your own Syphon, over your home network, or through your own VPN box. No accounts elsewhere, no analytics.
- **What stays on the computer:** it keeps one settings file in its data folder, readable only by your user. That file holds:
  - the node's addresses
  - this computer's own VPN key (only its public half is on the VPN box)
  - each person's "trusted device" cookie
  - the app settings
- **Remembered sign-in:** this means anyone at that computer can pick your name without the password, just like a streaming app on a TV. Leave it off on a shared computer.
- **Removing someone:** do it here or in Syphon under Settings > Account > Trusted devices. Either one ends it at once.

How it works inside:
- The window talks to a small helper on `127.0.0.1` that only the app's own window can use (a fresh secret every start).
- The helper keeps each person's cookies apart and passes requests to the node over the home network, through its built-in WireGuard (boringtun for WireGuard, smoltcp for TCP, both inside the app)
- The app itself has no access to anything else on the computer.

## Building

Everything is pinned: `rust-toolchain.toml` sets Rust and `Cargo.lock` sets every library.

**Linux** (Debian 12, Ubuntu 22.04 or newer):

    sudo apt install libwebkit2gtk-4.1-dev librsvg2-dev libxdo-dev libssl-dev build-essential pkg-config
    cargo install tauri-cli --version "^2" --locked
    cargo tauri build -- --locked

This makes `target/release/bundle/deb/Syphon_*.deb` and an AppImage.
- The .deb pulls in the video codecs (GStreamer) by itself.
- With the AppImage, install them yourself: `sudo apt install gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav`.
- **Blank or white window** (seen on Cinnamon with some graphics drivers): the app already switches off WebKit's newer drawing method that causes it. If the window is still blank, start it with `syphon-app --safe-graphics`, which also switches off compositing; video then uses a little more processor.

**Windows, on a Windows 10 or 11 PC** (64-bit). The first build takes 15–30 minutes; later ones are quicker.
1. **Visual Studio Build Tools:** download "Build Tools for Visual Studio 2022" from Microsoft, and in the installer tick **Desktop development with C++** (it brings the compiler and the Windows SDK). About 6–8 GB.
2. **Rust:** download and run `rustup-init.exe` from rustup.rs, and accept the defaults (it picks the MSVC toolchain). Close and reopen the terminal afterwards.
3. **The Tauri build tool:** in PowerShell, `cargo install tauri-cli --version "^2" --locked`
4. **Build:** unpack `syphon-app.tar.gz` (Windows 10/11 can: `tar -xzf syphon-app.tar.gz` in PowerShell), then in the `syphon-app` folder:

       cargo tauri build --bundles nsis -- --locked

   `rust-toolchain.toml` makes rustup fetch the exact Rust version by itself on the first build. Tauri downloads the NSIS installer maker by itself too, so the PC needs internet for the build.
5. **The installer** is `target\release\bundle\nsis\Syphon_0.2.0_x64-setup.exe`. Copy it to any Windows PC and run it. For an `.msi` as well, use `--bundles nsis,msi`.

On GitHub instead (later): push a tag like `v0.2.0`, and the workflow in `.github/workflows/build.yml` builds the Windows and Linux packages; they appear under the run's Artifacts.

**Windows notes:**
- The app needs Microsoft's WebView2, which Windows 10 and 11 already have; the installer fetches it if missing.
- The first time the app looks for your Syphon at home, Windows may ask whether it may use the network: allow it on **private networks** (home). Without that, finding the Syphon on the network and waking it at home don't work; the VPN still does.
- The installer is not code-signed, so Windows SmartScreen warns the first time: "More info" > "Run anyway". A code-signing certificate removes that, but it costs money every year.

## Tested

- **Unit tests:** addresses, connection codes, VPN codes, and keys checked against `wg pubkey`.
- **`tests/vpn_e2e.sh`** (run as root, next to the `syphon-vpn` source) runs the app against a real VPN box in three network namespaces (this computer, the box, the Syphon server). The box runs real WireGuard and its real firewall. It checks that:
  - the code's one-time key alone doesn't reach the server
  - first contact puts the app's own key in place, and the code can't be used twice
  - the whole app (its helper, without the window) takes a VPN code, keeps only its own key, connects through the box and passes pages and downloads through intact, also several at once
  - after a restart it reconnects by itself, and a device removed on the box is cut off with a clear message
  - a sleeping node (its web server only starts after a correct wake packet) is woken through the box when the app connects away from home, and by the app itself at home
- **Not tested:** real internet delays, real routers and the Windows build (that happens on GitHub's machines).

## Limits

- **Finding the node:** this works within one network segment. A node on another VLAN doesn't show up, so type its address instead.
- **Playback:** the app uses the computer's own video engine, so the node converts what that engine can't play, as it does for browsers.
- **Updates:** there is no automatic app update yet. Install the new version over the old one; your settings stay.
- **Hardware remotes:** HDMI-CEC (the TV's own remote) isn't supported, because most computers can't receive it. A cheap USB or Bluetooth remote works instead.

License: AGPL-3.0-only.
