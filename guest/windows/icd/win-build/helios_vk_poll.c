/*
 * helios_vk_poll.c — coherency probe: does a GPU write to HOST_VISIBLE|COHERENT
 * memory become visible to the CPU *polling in a loop* (as venus fence feedback
 * does), as opposed to a single read after a fence (helios_vk_dev/exec)?
 *
 * Writes a sentinel, submits vkCmdFillBuffer(0xCAFE0000) WITHOUT waiting on the
 * fence, then spins reading word[0] until it changes or a cap is hit. Then waits
 * the fence and reads once more. If the spin never sees the change but the
 * post-fence read does, the mapping is non-coherent for polling (stale CPU cache).
 *
 * Build: gcc -O2 -o C:\Users\Rupansh\helios_vk_poll.exe Z:\icd\win-build\helios_vk_poll.c -IZ:\icd\mesa\include
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#define ILOAD(n) (PFN_##n)(void *) gipa(inst, #n)
#define DLOAD(n) (PFN_##n)(void *) gdpa(dev, #n)
#define CK(e) do{VkResult _r=(e); if(_r){fprintf(stderr,"FAIL %s => %d\n",#e,_r);return 9;}}while(0)

int main(void){
   HMODULE vk=LoadLibraryW(L"vulkan-1.dll");
   PFN_vkGetInstanceProcAddr gipa=(PFN_vkGetInstanceProcAddr)(void*)GetProcAddress(vk,"vkGetInstanceProcAddr");
   PFN_vkCreateInstance ci_=(PFN_vkCreateInstance)gipa(NULL,"vkCreateInstance");
   VkApplicationInfo app={.sType=VK_STRUCTURE_TYPE_APPLICATION_INFO,.apiVersion=VK_API_VERSION_1_1};
   VkInstanceCreateInfo ici={.sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,.pApplicationInfo=&app};
   VkInstance inst; CK(ci_(&ici,NULL,&inst));
   PFN_vkEnumeratePhysicalDevices pe=ILOAD(vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties pq=ILOAD(vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkGetPhysicalDeviceMemoryProperties pm=ILOAD(vkGetPhysicalDeviceMemoryProperties);
   PFN_vkCreateDevice pcd=ILOAD(vkCreateDevice);
   PFN_vkGetDeviceProcAddr gdpa=ILOAD(vkGetDeviceProcAddr);
   uint32_t n=0; CK(pe(inst,&n,NULL)); VkPhysicalDevice pds[8]; if(n>8)n=8; CK(pe(inst,&n,pds));
   VkPhysicalDevice phys=pds[0];
   uint32_t qn=0; pq(phys,&qn,NULL); VkQueueFamilyProperties qf[16]; if(qn>16)qn=16; pq(phys,&qn,qf);
   uint32_t qfi=0; for(uint32_t i=0;i<qn;i++) if(qf[i].queueFlags&(VK_QUEUE_GRAPHICS_BIT|VK_QUEUE_COMPUTE_BIT)){qfi=i;break;}
   float pr=1; VkDeviceQueueCreateInfo qci={.sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,.queueFamilyIndex=qfi,.queueCount=1,.pQueuePriorities=&pr};
   VkDeviceCreateInfo dci={.sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,.queueCreateInfoCount=1,.pQueueCreateInfos=&qci};
   VkDevice dev; CK(pcd(phys,&dci,NULL,&dev));
   PFN_vkGetDeviceQueue gq=DLOAD(vkGetDeviceQueue);
   PFN_vkCreateBuffer cb=DLOAD(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements gr=DLOAD(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory am=DLOAD(vkAllocateMemory);
   PFN_vkBindBufferMemory bm=DLOAD(vkBindBufferMemory);
   PFN_vkMapMemory mm=DLOAD(vkMapMemory);
   PFN_vkCreateCommandPool ccp=DLOAD(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers acb=DLOAD(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer bc=DLOAD(vkBeginCommandBuffer);
   PFN_vkCmdFillBuffer cf=DLOAD(vkCmdFillBuffer);
   PFN_vkEndCommandBuffer ec=DLOAD(vkEndCommandBuffer);
   PFN_vkQueueSubmit qs=DLOAD(vkQueueSubmit);
   PFN_vkCreateFence cfn=DLOAD(vkCreateFence);
   PFN_vkWaitForFences wf=DLOAD(vkWaitForFences);
   VkQueue q; gq(dev,qfi,0,&q);
   VkBufferCreateInfo bci={.sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,.size=4096,.usage=VK_BUFFER_USAGE_TRANSFER_DST_BIT};
   VkBuffer buf; CK(cb(dev,&bci,NULL,&buf));
   VkMemoryRequirements rq; gr(dev,buf,&rq);
   VkPhysicalDeviceMemoryProperties mp; pm(phys,&mp);
   uint32_t mt=~0u; for(uint32_t i=0;i<mp.memoryTypeCount;i++){VkMemoryPropertyFlags f=mp.memoryTypes[i].propertyFlags;
      if((rq.memoryTypeBits&(1u<<i))&&(f&VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)&&(f&VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)){mt=i;break;}}
   VkMemoryAllocateInfo mai={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.allocationSize=rq.size,.memoryTypeIndex=mt};
   VkDeviceMemory mem; CK(am(dev,&mai,NULL,&mem)); CK(bm(dev,buf,mem,0));
   volatile uint32_t *p; CK(mm(dev,mem,0,4096,0,(void**)&p));
   p[0]=0xABABABAB;
   VkCommandPool pool; VkCommandPoolCreateInfo pci={.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,.queueFamilyIndex=qfi};
   CK(ccp(dev,&pci,NULL,&pool));
   VkCommandBuffer cmd; VkCommandBufferAllocateInfo cbi={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,.commandPool=pool,.level=VK_COMMAND_BUFFER_LEVEL_PRIMARY,.commandBufferCount=1};
   CK(acb(dev,&cbi,&cmd));
   VkCommandBufferBeginInfo bg={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,.flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
   CK(bc(cmd,&bg)); cf(cmd,buf,0,4096,0xCAFE0000u); CK(ec(cmd));
   VkFence fence; VkFenceCreateInfo fci={.sType=VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}; CK(cfn(dev,&fci,NULL,&fence));
   VkSubmitInfo si={.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,.commandBufferCount=1,.pCommandBuffers=&cmd};
   fprintf(stderr,"submitting (no wait), then spin-polling word[0] (was 0x%08x)...\n",p[0]); fflush(stderr);
   CK(qs(q,1,&si,fence));
   uint64_t spins=0; const uint64_t CAP=2000000000ull; uint32_t seen=p[0];
   while(seen!=0xCAFE0000u && spins<CAP){ seen=p[0]; spins++; }
   if(seen==0xCAFE0000u) fprintf(stderr,"POLL SAW the GPU write after %llu spins -> mapping IS coherent for polling\n",(unsigned long long)spins);
   else fprintf(stderr,"POLL NEVER saw write (still 0x%08x after %llu spins) -> NON-coherent for polling\n",seen,(unsigned long long)spins);
   fflush(stderr);
   wf(dev,1,&fence,VK_TRUE,5000000000ull);
   fprintf(stderr,"after vkWaitForFences, single read word[0] = 0x%08x\n",p[0]);
   return 0;
}
