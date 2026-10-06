/* What Vulkan Video a device offers: queue families with their video codec
 * operations, the video extensions, and the H.264 decode capabilities and
 * output formats. Honours VK_DIRECT_DRIVER (see vk_direct_driver.h).
 *
 *   vk_video_probe
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vk_direct_driver.h"

static void
probe_h264(VkInstance inst, VkPhysicalDevice pd)
{
   PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR get_caps =
      (PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR)
      vkGetInstanceProcAddr(inst, "vkGetPhysicalDeviceVideoCapabilitiesKHR");
   PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR get_fmts =
      (PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR)
      vkGetInstanceProcAddr(inst, "vkGetPhysicalDeviceVideoFormatPropertiesKHR");
   if (!get_caps || !get_fmts) {
      printf("\tno vkGetPhysicalDeviceVideoCapabilitiesKHR\n");
      return;
   }

   static const StdVideoH264ProfileIdc profiles[] = {
      STD_VIDEO_H264_PROFILE_IDC_BASELINE, STD_VIDEO_H264_PROFILE_IDC_MAIN,
      STD_VIDEO_H264_PROFILE_IDC_HIGH,
   };
   for (unsigned i = 0; i < 3; i++) {
      VkVideoDecodeH264ProfileInfoKHR h264 = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PROFILE_INFO_KHR,
         .stdProfileIdc = profiles[i],
         .pictureLayout = VK_VIDEO_DECODE_H264_PICTURE_LAYOUT_PROGRESSIVE_KHR,
      };
      VkVideoProfileInfoKHR prof = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_PROFILE_INFO_KHR, .pNext = &h264,
         .videoCodecOperation = VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR,
         .chromaSubsampling = VK_VIDEO_CHROMA_SUBSAMPLING_420_BIT_KHR,
         .lumaBitDepth = VK_VIDEO_COMPONENT_BIT_DEPTH_8_BIT_KHR,
         .chromaBitDepth = VK_VIDEO_COMPONENT_BIT_DEPTH_8_BIT_KHR,
      };
      VkVideoDecodeH264CapabilitiesKHR h264caps = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_CAPABILITIES_KHR };
      VkVideoDecodeCapabilitiesKHR dcaps = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR,
         .pNext = &h264caps };
      VkVideoCapabilitiesKHR caps = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_CAPABILITIES_KHR, .pNext = &dcaps };
      VkResult r = get_caps(pd, &prof, &caps);
      printf("\tH.264 profile_idc %d: %d", profiles[i], r);
      if (r == VK_SUCCESS) {
         printf(" max %ux%u dpb %u refs %u level %d dec_flags 0x%x",
                caps.maxCodedExtent.width, caps.maxCodedExtent.height,
                caps.maxDpbSlots, caps.maxActiveReferencePictures,
                h264caps.maxLevelIdc, dcaps.flags);
      }
      printf("\n");

      if (r != VK_SUCCESS || i != 2)
         continue;

      VkVideoProfileListInfoKHR list = {
         .sType = VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR,
         .profileCount = 1, .pProfiles = &prof };
      VkPhysicalDeviceVideoFormatInfoKHR fi = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_FORMAT_INFO_KHR,
         .pNext = &list,
         .imageUsage = VK_IMAGE_USAGE_VIDEO_DECODE_DST_BIT_KHR |
                       VK_IMAGE_USAGE_VIDEO_DECODE_DPB_BIT_KHR };
      uint32_t n = 0;
      get_fmts(pd, &fi, &n, NULL);
      VkVideoFormatPropertiesKHR fp[8];
      for (unsigned k = 0; k < 8; k++)
         fp[k] = (VkVideoFormatPropertiesKHR) {
            .sType = VK_STRUCTURE_TYPE_VIDEO_FORMAT_PROPERTIES_KHR };
      if (n > 8)
         n = 8;
      get_fmts(pd, &fi, &n, fp);
      for (unsigned k = 0; k < n; k++)
         printf("\t  decode format %d tiling %d usage 0x%x\n", fp[k].format,
                fp[k].imageTiling, fp[k].imageUsageFlags);
   }
}

int main(void)
{
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk_video_probe", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app };
   direct_driver_chain(&ici);
   VkInstance inst;
   VkResult r = vkCreateInstance(&ici, NULL, &inst);
   if (r != VK_SUCCESS) {
      printf("vkCreateInstance: %d\n", r);
      return 1;
   }

   uint32_t n = 0;
   vkEnumeratePhysicalDevices(inst, &n, NULL);
   VkPhysicalDevice pds[8];
   if (n > 8)
      n = 8;
   vkEnumeratePhysicalDevices(inst, &n, pds);
   for (uint32_t d = 0; d < n; d++) {
      VkPhysicalDeviceDriverProperties drv = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
      VkPhysicalDeviceProperties2 p2 = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &drv };
      vkGetPhysicalDeviceProperties2(pds[d], &p2);
      printf("GPU%u: %s (%s %s)\n", d, p2.properties.deviceName,
             drv.driverName, drv.driverInfo);

      uint32_t qn = 0;
      vkGetPhysicalDeviceQueueFamilyProperties2(pds[d], &qn, NULL);
      VkQueueFamilyProperties2 qf[16];
      VkQueueFamilyVideoPropertiesKHR qv[16];
      if (qn > 16)
         qn = 16;
      for (uint32_t i = 0; i < qn; i++) {
         qv[i] = (VkQueueFamilyVideoPropertiesKHR) {
            .sType = VK_STRUCTURE_TYPE_QUEUE_FAMILY_VIDEO_PROPERTIES_KHR };
         qf[i] = (VkQueueFamilyProperties2) {
            .sType = VK_STRUCTURE_TYPE_QUEUE_FAMILY_PROPERTIES_2,
            .pNext = &qv[i] };
      }
      vkGetPhysicalDeviceQueueFamilyProperties2(pds[d], &qn, qf);
      int video_queue = 0;
      for (uint32_t i = 0; i < qn; i++) {
         const VkQueueFlags f = qf[i].queueFamilyProperties.queueFlags;
         printf("\tqueue family %u: flags 0x%x count %u video ops 0x%x%s%s\n",
                i, f, qf[i].queueFamilyProperties.queueCount,
                qv[i].videoCodecOperations,
                (f & VK_QUEUE_VIDEO_DECODE_BIT_KHR) ? " DECODE" : "",
                (f & VK_QUEUE_VIDEO_ENCODE_BIT_KHR) ? " ENCODE" : "");
         if (f & (VK_QUEUE_VIDEO_DECODE_BIT_KHR | VK_QUEUE_VIDEO_ENCODE_BIT_KHR))
            video_queue = 1;
      }

      uint32_t en = 0;
      vkEnumerateDeviceExtensionProperties(pds[d], NULL, &en, NULL);
      VkExtensionProperties *ext = calloc(en, sizeof(*ext));
      vkEnumerateDeviceExtensionProperties(pds[d], NULL, &en, ext);
      unsigned vn = 0;
      for (uint32_t i = 0; i < en; i++) {
         if (strstr(ext[i].extensionName, "video")) {
            printf("\t%s\n", ext[i].extensionName);
            vn++;
         }
      }
      printf("\t%u video extensions of %u\n", vn, en);
      free(ext);

      if (video_queue)
         probe_h264(inst, pds[d]);
   }

   vkDestroyInstance(inst, NULL);
   return 0;
}
