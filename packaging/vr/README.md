# Packaged VR components

`SteamVR/drivers/openrigcue` is OpenRigCue's native motion-compensation driver. Register its folder with
SteamVR's `vrpathreg adddriver`. Its resources include the configuration example.

`SteamVR/drivers/simracingstudio` is the Rust/Linux port of the original virtual-tracker driver. Register its
folder to use the original tracker's interfaces and motion-rig-pose mapping.

`OpenXR` contains the native motion-compensation layer and its manifest template. Copy the library to a
persistent location, replace `@LIBRARY_PATH@` with that library's absolute path, and save the manifest as
`openrigcue_xr_layer.json` under `$XDG_DATA_HOME/openxr/1/api_layers/implicit.d` (default
`~/.local/share/openxr/1/api_layers/implicit.d`). Enable it per application with
`OPENRIGCUE_XR_LAYER_ENABLE=1`; `OPENRIGCUE_XR_LAYER_DISABLE=1` disables it.

The package build stages these files. It does not register drivers or change the user's VR configuration.
