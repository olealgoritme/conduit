# Conduit: use the host's NVIDIA user-space, shared read-only at /mnt/nvidia.
NVGPU_SHARE=/mnt/nvidia
# Vulkan ICD manifest: without it the loader finds only the CPU rasteriser.
export VK_DRIVER_FILES=$NVGPU_SHARE/share/vulkan/icd.d/nvidia_icd.json
# GLVND EGL vendor config: libGLX_nvidia hands out no Vulkan entry points until
# its EGL side has found an NVIDIA vendor, and the share is not on the default path.
export __EGL_VENDOR_LIBRARY_DIRS=$NVGPU_SHARE/share/glvnd/egl_vendor.d
# NVIDIA's platform glue from the share first (egl-wayland2, GBM, X11); the
# distribution's next, so an installed egl-wayland (v1) is a fallback.
export __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=$NVGPU_SHARE/share/egl/egl_external_platform.d:/usr/share/egl/egl_external_platform.d
# NVIDIA's backend first; the distribution's next, for the emulated display
# card an attached VM also has (its GBM device would fail without Mesa's).
export GBM_BACKENDS_PATH=$NVGPU_SHARE/lib/gbm:/usr/lib/x86_64-linux-gnu/gbm:/usr/lib/gbm GBM_BACKEND=nvidia-drm
# Hyprland: render on Conduit's card, not the emulated one (99-conduit.rules).
export AQ_DRM_DEVICES=/dev/dri/conduit-card
case ":$PATH:" in *:$NVGPU_SHARE/bin:*) ;; *) export PATH=$PATH:$NVGPU_SHARE/bin ;; esac
