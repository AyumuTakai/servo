> [!NOTE]
> **About this fork (AyumuTakai/servo)**
>
> This fork carries changes to Servo made for **Kyousei OS**, a desktop environment that embeds Servo in its own
> Wayland compositor. The `main` branch is not used for that work; the changes live on the `kyousei/*` branches below.
> They are kept here, in public, as the source of the modified files (Servo is MPL-2.0).
>
> | Branch | Base | What it contains |
> |---|---|---|
> | [`kyousei/v0.7.0-upstreamable`](https://github.com/AyumuTakai/servo/tree/kyousei/v0.7.0-upstreamable) | `v0.7.0` | **Fixes for Servo itself**: Servo bugs and missing standard behaviour that any embedder would hit (caret and editing in `contenteditable`, selection, IME, hit testing, `Range` rectangles, mouse events, `<svg>` flicker and memory, video frames, and more). One commit per topic, each explaining the problem and the fix. |
> | [`kyousei/v0.7.0-kyousei-only`](https://github.com/AyumuTakai/servo/tree/kyousei/v0.7.0-kyousei-only) | `kyousei/v0.7.0-upstreamable` | The branch above plus **features and policies only Kyousei OS needs** as an embedder: downloads to files, saving cookies while running, keeping the VA-API GStreamer plugins, the body and origin of intercepted requests, and `navigator.hostChannel` (a page-to-embedder channel). |
> | [`kyousei/v0.7.0-work`](https://github.com/AyumuTakai/servo/tree/kyousei/v0.7.0-work) | `v0.7.0` | The **working branch**: the same changes in the order they were made (including temporary debugging commits). This is what Kyousei OS builds from. |
>
> - The tip of `kyousei/v0.7.0-kyousei-only` has the same tree as a commit of the working branch: the two
>   reorganized branches are the same changes, regrouped to be read. Each of their commits lists the working-branch
>   commits it came from (`Original commits:`).
> - The branches only grow (no force pushes). Released states are tagged `kyousei/v<Servo version>-r<n>`.
> - When Servo makes a new release, new `kyousei/v<new version>-*` branches are made from its tag; the old ones are kept.
> - These changes were written with the help of an AI model. Servo does not accept AI-generated contributions, so
>   they are **not** submitted upstream (no pull requests or issues); they are published here so that anyone can read them.

# The Servo Parallel Browser Engine Project

Servo is a prototype web browser engine written in the
[Rust](https://github.com/rust-lang/rust) language. It is currently developed on
64-bit macOS, 64-bit Linux, 64-bit Windows, 64-bit OpenHarmony, and Android.

Servo welcomes contribution from everyone. Check out:

- The [Servo Book](https://book.servo.org) for documentation
- [servo.org](https://servo.org/) for news and guides

Coordination of Servo development happens:
- Here in the Github Issues
- On the [Servo Zulip](https://servo.zulipchat.com/)
- In video calls advertised in the [Servo Project](https://github.com/servo/project/issues) repo.

## Getting started

For more detailed build instructions, see the Servo Book under [Getting the Code] and [Building Servo].

[Getting the Code]: https://book.servo.org/building/getting-the-code.html
[Building Servo]: https://book.servo.org/building/building.html

### macOS

- Download and install [Xcode](https://developer.apple.com/xcode/) and [`brew`](https://brew.sh/).
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Linux

- Install `curl`:
  - Arch: `sudo pacman -S --needed curl`
  - Debian, Ubuntu: `sudo apt install curl`
  - Fedora: `sudo dnf install curl`
  - Gentoo: `sudo emerge net-misc/curl`
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Windows

- Download [`uv`](https://docs.astral.sh/uv/getting-started/installation/#standalone-installer), and [`rustup`](https://win.rustup.rs/)
  - Be sure to select *Quick install via the Visual Studio Community installer*
- Ensure that [`winget`](https://learn.microsoft.com/en-us/windows/package-manager/winget/) is available. It should be preinstalled on Windows 10 1809+ and Windows 11, otherwise can be [`manually installed`](https://github.com/microsoft/winget-cli#installing-the-client).
- In the Visual Studio Installer, ensure the following components are installed:
  - **Windows 10/11 SDK (anything >= 10.0.19041.0)** (`Microsoft.VisualStudio.Component.Windows{10, 11}SDK.{>=19041}`)
  - **MSVC v143 - VS 2022 C++ x64/x86 build tools (Latest)** (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`)
  - **C++ ATL for latest v143 build tools (x86 & x64)** (`Microsoft.VisualStudio.Component.VC.ATL`)
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `.\mach bootstrap`
- Build servoshell: `.\mach build`

### Android

- Ensure that the following environment variables are set:
  - `ANDROID_SDK_ROOT`
  - `ANDROID_NDK_ROOT`: `$ANDROID_SDK_ROOT/ndk/28.2.13676358/`
 `ANDROID_SDK_ROOT` can be any directory (such as `~/android-sdk`).
  All of the Android build dependencies will be installed there.
- Install the latest version of the [Android command-line
  tools](https://developer.android.com/studio#command-tools) to
  `$ANDROID_SDK_ROOT/cmdline-tools/latest`.
- Run the following command to install the necessary components:
  ```shell
  sudo $ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager --install \
   "build-tools;36.0.0" \
   "emulator" \
   "ndk;29.0.14206865" \
   "platform-tools" \
   "platforms;android-37" \
   "system-images;android-37;google_apis;x86_64"
  ```
- Follow the instructions above for the platform you are building on

### OpenHarmony

- Follow the instructions above for the platform you are building on to prepare the environment.
- Depending on the target distribution (e.g. `HarmonyOS NEXT` vs pure `OpenHarmony`) the build configuration will differ slightly.
- Ensure that the following environment variables are set
  - `DEVECO_SDK_HOME` (Required when targeting `HarmonyOS NEXT`)
  - `OHOS_BASE_SDK_HOME` (Required when targeting `OpenHarmony`)
  - `OHOS_SDK_NATIVE` (e.g. `${DEVECO_SDK_HOME}/default/openharmony/native` or `${OHOS_BASE_SDK_HOME}/${API_VERSION}/native`)
  - `SERVO_OHOS_SIGNING_CONFIG`: Path to json file containing a valid signing configuration for the demo app.
- Review the detailed instructions at [Building for OpenHarmony].
- The target distribution can be modified by passing `--flavor=<default|harmonyos>` to `mach <build|package|install>`.
