# Piwaku on Windows

## Install

Piwaku's current public release workflow publishes macOS installers only.
Windows builds can still be packaged locally with the repository's build
script. Run the resulting installer when one is available. It
installs per-user into `%LOCALAPPDATA%\Programs\Waku`, so it never asks for
administrator rights.

### Portable

`waku-<version>-<target>.zip` is the same build without an installer. Unpack it
anywhere and run `waku.exe`.

**Keep the two executables together.** Waku launches `waku-daemon.exe` from its
own directory, so moving `waku.exe` out on its own leaves it unable to start
the daemon. A shortcut is fine.

A portable copy can be replaced manually by unpacking a newer local build over
the existing directory after closing the app.

Waku expects:

- **Windows 10 version 1809 or newer**, or Windows 11.
- **A Direct3D 11 driver at feature level 11_0 or newer.** GPUI renders
  through DirectX and falls back to the Microsoft Basic Render Driver, so it
  can run in a VM — see Troubleshooting if the window comes up black.
- **x86_64 or aarch64.**

Nothing else: Waku links the C runtime statically, so there is no Visual C++
redistributable to install first. That matters most on Arm devices, which
rarely have the arm64 redistributable already.

SmartScreen may warn about an unrecognized publisher on first launch when the
release was not code-signed. Choose **More info → Run anyway**.

## Updating

Piwaku has no in-app updater. Close the app and install a newer local build
manually, or use the package manager that installed it.

## Where Waku keeps its data

| What | Path |
| --- | --- |
| Tasks, sessions, transcripts | `%LOCALAPPDATA%\Waku\app.db` |
| Attachments and blobs | `%LOCALAPPDATA%\Waku\blobs` |
| Settings | `%USERPROFILE%\.waku\app.json` |

Unpacking a new release over the old directory leaves all of it untouched.

## Agent CLIs

Waku detects the provider CLIs on `PATH` and, because a fresh `PATH` may
predate an install, also looks in the usual per-user prefixes:
`%APPDATA%\npm`, `%USERPROFILE%\.bun\bin`, `%USERPROFILE%\.cargo\bin`,
`%USERPROFILE%\scoop\shims`, and `%LOCALAPPDATA%\Microsoft\WindowsApps`.

Bare names resolve through `PATHEXT`, so the `claude.cmd` shim npm installs is
found the same way `claude` would be in a shell. Nothing is spawned with a
console window attached.

If a CLI is installed but not detected, set its path explicitly in
**Settings → Providers**.

## Terminal

The built-in terminal opens PowerShell 7 (`pwsh.exe`) when it is installed,
then Windows PowerShell, then whatever `COMSPEC` names. Ctrl+Shift+C and
Ctrl+Shift+V copy and paste so Ctrl+C stays available to the shell.

## Browser

The right panel's Browser tab runs on WebView2, which is in-box on Windows 11
and evergreen-installed on Windows 10. Navigation, devtools, downloads, and
pop-up handling behave as they do on macOS.

Waku hosts it in *visual* mode rather than as a child window: the page renders
into a DirectComposition visual that GPUI hands out between its own content
and its overlay plane, so menus, tooltips and dialogs composite above a live
page instead of hiding it. That is also why the browser needs a working
composition path — see the black-window note under Troubleshooting.

Differences worth knowing:

- **No load progress in the toolbar.** WebView2 reports no equivalent of
  WebKit's `estimatedProgress`, so the bar stays empty while a page loads.
- **Devtools open but do not toggle.** WebView2 offers no way to ask whether
  its devtools window is open, or to close it, so the shortcut only opens and
  refocuses it.
- **Pen, touch and dragging files into the page are not wired up.** Visual
  hosting delivers no input of its own; Waku forwards mouse, wheel, cursor and
  focus, and leaves `SendPointerInput` and the external drop target for later.
  Keyboard and IME are unaffected — those still reach the page directly once
  it holds focus.

## What is not available yet

- **Computer use.** The runtime and its UI stay disabled off macOS.
- **Terminals over the daemon's browser client.** The desktop terminal works;
  a remote browser client connected to a Windows daemon cannot open one.

## Troubleshooting

**The window opens black, or the app exits at startup.** Waku needs a working
Direct3D 11 device. Update the GPU driver; in a VM, enable 3D acceleration.

**A provider is listed as not installed.** Open a new PowerShell window and run
the CLI by name. If the shell cannot find it either, the install did not put a
shim on `PATH`. If the shell finds it but Waku does not, set the binary path in
**Settings → Providers** and file an issue with the install method.

**Git-backed features do nothing.** Waku shells out to `git`. Install Git for
Windows and make sure `git --version` works in a new terminal.

Piwaku has no background update feed. Install a newer local build manually
after closing the app.
