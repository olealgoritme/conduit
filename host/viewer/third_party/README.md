# Vendored shader sources

Used by the viewer's upscalers (docs/VIEWER-ENHANCE.md). All MIT; each
directory has its license. The files are byte-for-byte the upstream ones.

| Directory | What | Upstream |
|---|---|---|
| `fsr1/` | `ffx_a.h`, `ffx_fsr1.h`: AMD FidelityFX Super Resolution 1 (EASU, RCAS), v1.20210629 | github.com/GPUOpen-Effects/FidelityFX-FSR, `ffx-fsr/` |
| `cas/` | `ffx_cas.h`: AMD FidelityFX Contrast Adaptive Sharpening, 1.20190610 | github.com/GPUOpen-Effects/FidelityFX-CAS, `ffx-cas/` |
| `nis/` | `NIS_Scaler.h`, `NIS_Config.h`: NVIDIA Image Scaling SDK v1.0.3 | github.com/NVIDIAGameWorks/NVIDIAImageScaling, `NIS/` |

How they are used:

- `shaders/embed.sh` embeds the FidelityFX headers into the viewer as they
  are, except four constant-setup statements that assign an `int` literal
  to a `uint` (desktop GLSL converts implicitly, GLSL ES does not), which get
  a `u` suffix.
- `shaders/nis_scaler_gles.glsl` is a GLSL ES 3.10 port of `NIS_Scaler.h`
  (its header says what changed); `shaders/nis_coef.h` holds the filter
  coefficients from `NIS_Config.h` as C arrays, and `nb_enhance.h`
  `nb_nis_config()` ports `NVScalerUpdateConfig()`.
