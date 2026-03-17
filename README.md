# DXGI Proxy VSync Uncapper

A lightweight DLL proxy for `dxgi.dll` written in Rust that forcibly disables VSync (Vertical Synchronization) and uncaps framerates in Direct3D 11 and Direct3D 12 games. It is specifically designed to work with modern Windows Flip Model presentations (like UWP apps, GDK, and Minecraft Bedrock Edition) where standard overriding tools fail.

## How it Works

Modern DirectX 12 and UWP games are strictly bound by the Desktop Window Manager (DWM). Simply intercepting the `Present` command and changing the SyncInterval to `0` is ignored by the DWM unless the application's underlying Swap Chain was explicitly built with permission to drop frames.

This proxy works by performing a "Man-in-the-Middle" attack on the game engine:
1. **At Startup**: It loads the real `C:\Windows\System32\dxgi.dll` to ensure no genuine DirectX functionality is broken.
2. **At Swap Chain Creation**: It intercepts the `CreateSwapChain` commands before they reach DXGI, secretly injecting the `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` flag into the engine's configuration.
3. **During Resize (F11 / Fullscreen)**: DirectX strictly enforces that `ResizeBuffers` must include the tearing flag if the swap chain was created with it. To prevent the game from crashing, the proxy automatically re-injects the flag during every window resize event.
4. **During Presentation**: It forces `SyncInterval=0` and appends `DXGI_PRESENT_ALLOW_TEARING` to every frame delivery.

Because it only modifies factory creation flags and presentation parameters, it is **100% compatible with third-party overlays** (like MSI Afterburner, Discord, and Steam), which simply hook the same vtable further down the chain.

## Usage

1. **Build the DLL**:
   Run the following command in the project directory:
   ```bash
   cargo build --release
   ```

2. **Locate the Output**:
   The compiled DLL will be located at `target/release/dxgi_proxy.dll`.

3. **Install the Proxy**:
   Rename `dxgi_proxy.dll` to **`dxgi.dll`** and drop it directly next to the `.exe` of the game you want to uncap (e.g., next to the Minecraft Bedrock application executable).

4. **Verify it's working**:
   When the game launches, it will spawn a log file located at `%TEMP%\dxgi_proxy.log`. You can read this file to verify that the DXGI Factory was intercepted, the tearing flags were successfully injected, and that the `Present` commands are firing. 

## Requirements
* Rust and Cargo (to build from source)
* Windows 10/11 (Flip Model tearing requires a relatively modern Windows build)

## Disclaimer
Modifying rendering queues and overriding engine VSync constraints may cause screen tearing or inconsistent frame delivery depending on the game engine's internal physics/tick rate locking. Use at your own discretion.
