//! The Venus half of the guest-memory blob Blt destination (`GuestBlob`): importing a virtio-gpu
//! GUEST blob into the KMD's own Venus device as a plain TRANSFER_DST `VkBuffer`, the reusable
//! copy into it, and its retirement. The decisions and every host-facing constant are
//! `helios_kmd_logic::guest_blob` (its `contract` module); the knob, counters, creation and the
//! paging-path teardown are `ddi/guest_blob.rs`. `docs/zero-copy-present.md` section 24.12.
//!
//! PASSIVE_LEVEL, under the Venus mutex, like every other function in the module.

use helios_kmd_logic::guest_blob::{contract as gbc, deadline, Why};

use super::ring::*;
use super::*;

/// Guest buffers alive at once: one per standard destination at most, bounded like the
/// Present-buffer cache.
const MAX_GUEST_BUFFERS: usize = MAX_PRESENT_BUFFERS;

/// One imported guest blob: the copy destination of the KMD standard buffer `destination`.
#[derive(Clone, Copy)]
pub(crate) struct GuestBuffer {
    /// The KMD standard buffer's Venus resource id (the Present destination).
    pub(super) destination: u32,
    /// The guest blob's resource id (the cache key of the copies into it).
    pub(super) guest: u32,
    pub(super) buffer_id: VkBufferId,
    pub(super) memory_id: VkDeviceMemoryId,
    /// The blob's (and the buffer's) size: `pitch * height` rounded up to pages.
    pub(super) size: u64,
    /// Retired: no new copy goes to it. Set first by [`VenusClient::retire_guest_buffer`];
    /// stays set on a record whose release failed (its objects are then kept for context
    /// teardown, and the pages stay pinned).
    pub(super) retired: bool,
}

/// Why an import failed, and whether everything it made was released again (the caller may
/// then unref the blob and unlock the pages) or not (the caller must keep them).
pub(crate) struct ImportFailed {
    pub why: Why,
    pub clean: bool,
}

impl VenusClient {
    /// The live guest buffer `desc` copies into, if any: not retired, large enough for the
    /// copy's extent (`pitch * (height - 1) + width * bpp`), and still the copy target of the
    /// destination's record (`Record::copy_target` naming this blob; one spinlock, only when a
    /// guest buffer of the destination exists). The record matters when a retire could not
    /// reach this client (its mutex wait ran out, the client was gone): the record is then
    /// poisoned and no longer a copy target, while this buffer was never marked retired. The
    /// ONE predicate every Present arm uses, under this client's mutex, so the arm's
    /// mirror/stale decision and the copy cannot disagree.
    pub(super) fn guest_target_for(
        &self,
        adapter: &AdapterContext,
        desc: &PresentBufferDesc,
    ) -> Option<GuestBuffer> {
        let extent = u64::from(desc.pitch)
            .checked_mul(u64::from(desc.height.checked_sub(1)?))?
            .checked_add(u64::from(desc.width) * u64::from(desc.pixel_format.bytes_per_pixel()))?;
        let candidate = self
            .guest_buffers
            .iter()
            .find(|g| g.destination == desc.resource_id && !g.retired && g.size >= extent)
            .copied()?;
        adapter
            .system_backings
            .guest_record(desc.resource_id)
            .is_some_and(|record| record.copy_target() && record.guest == candidate.guest)
            .then_some(candidate)
    }

    /// `vkGetMemoryResourcePropertiesMESA(guest)`: the resource's `memoryTypeBits`.
    fn guest_resource_memory_bits(
        &mut self,
        adapter: &AdapterContext,
        guest: u32,
    ) -> Result<u32, Why> {
        let w = gbc::encode_get_resource_properties(self.device_id.into(), guest);
        let stream = w.as_slice().map_err(|_| Why::Kmd)?;
        let mut r = self
            .ring_command_expect(
                adapter,
                stream,
                ReplyCheck::new(gbc::CMD_GET_MEMORY_RESOURCE_PROPERTIES_MESA),
            )
            .map_err(|_| Why::ImportOther)?;
        let result = r.read_i32().map_err(|_| Why::ImportOther)?;
        if result != 0 {
            return Err(gbc::classify_import(result));
        }
        if r.read_u64().map_err(|_| Why::ImportOther)? == 0 {
            return Err(Why::ImportOther);
        }
        if r.read_u32().map_err(|_| Why::ImportOther)?
            != gbc::ST_MEMORY_RESOURCE_PROPERTIES_MESA as u32
        {
            return Err(Why::ImportOther);
        }
        // pNext: none was asked for, none may come back.
        if r.read_u64().map_err(|_| Why::ImportOther)? != 0 {
            return Err(Why::ImportOther);
        }
        r.read_u32().map_err(|_| Why::ImportOther)
    }

    /// A plain TRANSFER_DST `VkBuffer` of `size` bytes (no external-memory struct: the host
    /// imports the guest blob with its own handle type).
    fn create_guest_destination_buffer(
        &mut self,
        adapter: &AdapterContext,
        size: u64,
    ) -> Result<VkBufferId, VirtioError> {
        let buffer_id = self.new_buffer_id();
        let w = gbc::encode_create_buffer(self.device_id.into(), buffer_id.into(), size);
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(gbc::CMD_CREATE_BUFFER).refuse_result_undiagnosed(),
        )?;
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            return Err(VirtioError::DeviceError);
        }
        Ok(buffer_id)
    }

    /// `vkAllocateMemory` with `VkImportMemoryResourceInfoMESA { guest }`, reading the
    /// `VkResult` itself so the failure class can be told apart.
    fn import_guest_memory(
        &mut self,
        adapter: &AdapterContext,
        guest: u32,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, Why> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &MemoryAllocateSpec {
                pnext: MemoryPNext::ImportResource { resource_id: guest },
                size,
                memory_type_index,
            },
        );
        let stream = w.as_slice().map_err(|_| Why::Kmd)?;
        let mut r = self
            .ring_command_expect(adapter, stream, ReplyCheck::new(CMD_ALLOCATE_MEMORY))
            .map_err(|_| Why::ImportOther)?;
        let result = r.read_i32().map_err(|_| Why::ImportOther)?;
        if result != 0 {
            return Err(gbc::classify_import(result));
        }
        Ok(memory_id)
    }

    /// Submit an empty fence marker and wait for it: every earlier ring command has executed
    /// and every earlier queue submission has completed when this returns `Ok`. The host waits
    /// at most [`deadline::FENCE_MS`] (`vkWaitForFences`), so its ring is never blocked longer;
    /// the guest side is bounded by the caller's section (`escape_wait::begin_bounded`).
    fn guest_queue_marker(&mut self, adapter: &AdapterContext) -> Result<(), VirtioError> {
        let fence = self.create_fence(adapter)?;
        self.queue_submit_fence_marker(adapter, fence)?;
        let ns = u64::from(deadline::fence_wait_ms(
            crate::ddi::escape_wait::bounded_left_ms(),
        )) * 1_000_000;
        self.wait_for_fence_within(adapter, fence, ns)?;
        self.destroy_fence(adapter, fence)
    }

    /// Undo a partial import: destroy the buffer, free the memory, and fence after the free
    /// (the free is asynchronous to the UNREF that follows). `true` when all of it succeeded.
    fn unwind_guest_import(
        &mut self,
        adapter: &AdapterContext,
        buffer_id: Option<VkBufferId>,
        memory_id: Option<VkDeviceMemoryId>,
    ) -> bool {
        let mut ok = true;
        if let Some(buffer_id) = buffer_id {
            ok &= self.destroy_buffer_on_ring(adapter, buffer_id).is_ok();
        }
        if let Some(memory_id) = memory_id {
            ok &= self.free_memory_object(adapter, memory_id).is_ok();
        }
        ok && self.guest_queue_marker(adapter).is_ok()
    }

    /// Import the GUEST blob `guest` (`size` bytes, already created on the host over the
    /// leased pages of `destination`) as the copy destination of `destination`.
    ///
    /// The host contract: the resource's memory types come from
    /// `vkGetMemoryResourcePropertiesMESA`; the type is the first HOST_VISIBLE|HOST_COHERENT
    /// one among them; `allocationSize` is the blob size; the buffer is a plain TRANSFER_DST
    /// buffer bound at offset 0. The memory is never mapped through Venus.
    pub(crate) fn import_guest_blob(
        &mut self,
        adapter: &AdapterContext,
        destination: u32,
        guest: u32,
        size: u64,
    ) -> Result<(), ImportFailed> {
        let failed = |why| ImportFailed { why, clean: true };
        if self
            .guest_buffers
            .iter()
            .any(|g| g.destination == destination)
        {
            // An older record (retired, its release failed) still names this destination.
            return Err(failed(Why::Busy));
        }
        if self.guest_buffers.len() >= MAX_GUEST_BUFFERS
            || (self.guest_buffers.len() == self.guest_buffers.capacity()
                && self.guest_buffers.try_reserve(4).is_err())
        {
            return Err(failed(Why::Kmd));
        }
        let bits = self
            .guest_resource_memory_bits(adapter, guest)
            .map_err(failed)?;
        crate::diag::record_named_bytes(b"VnGbBits", bits);
        let Some(memory_type_index) =
            gbc::choose_memory_type(&self.memory_type_flags, self.memory_type_count, bits)
        else {
            return Err(failed(Why::NoMemoryType));
        };
        let buffer_id = self
            .create_guest_destination_buffer(adapter, size)
            .map_err(|_| failed(Why::ImportOther))?;
        let requirements = self.buffer_memory_requirements(adapter, buffer_id);
        let fits = matches!(
            requirements,
            Ok((required, _, type_bits, _))
                if required <= size && type_bits & (1u32 << memory_type_index) != 0
        );
        if !fits {
            let clean = self.unwind_guest_import(adapter, Some(buffer_id), None);
            return Err(ImportFailed {
                why: Why::ImportOther,
                clean,
            });
        }
        let memory_id = match self.import_guest_memory(adapter, guest, size, memory_type_index) {
            Ok(id) => id,
            Err(why) => {
                // A refused allocation made no memory object.
                let clean = self.unwind_guest_import(adapter, Some(buffer_id), None);
                return Err(ImportFailed { why, clean });
            }
        };
        if self
            .bind_buffer_memory(adapter, buffer_id, memory_id)
            .is_err()
        {
            let clean = self.unwind_guest_import(adapter, Some(buffer_id), Some(memory_id));
            return Err(ImportFailed {
                why: Why::ImportOther,
                clean,
            });
        }
        // Capacity was reserved above: this push does not allocate.
        self.guest_buffers.push(GuestBuffer {
            destination,
            guest,
            buffer_id,
            memory_id,
            size,
            retired: false,
        });
        Ok(())
    }

    /// Retire the guest buffer of `destination` (guest blob `guest`), in the host's order:
    ///
    /// 1. no new copy targets it (`retired`);
    /// 2. drain: the wire fence of the last copy of every cached command into it, then a queue
    ///    fence marker (each at most `deadline::FENCE_MS`, all of it inside the caller's
    ///    bounded section of `deadline::DRAIN_MS`; PASSIVE, the paging thread may be the
    ///    caller);
    /// 3. release the cached copy commands;
    /// 4. `vkDestroyBuffer` + `vkFreeMemory`;
    /// 5. a fence after the free (Venus ring commands are asynchronous to the UNREF).
    ///
    /// The caller then sends `RESOURCE_UNREF`, and only after that unlocks the pages. `Ok`
    /// when there is nothing to release (the import never completed). On `Err` the record
    /// stays (retired) with every object it still owns, for context teardown.
    pub(crate) fn retire_guest_buffer(
        &mut self,
        adapter: &AdapterContext,
        destination: u32,
        guest: u32,
    ) -> Result<(), Why> {
        let Some(index) = self
            .guest_buffers
            .iter()
            .position(|g| g.destination == destination && g.guest == guest)
        else {
            return Ok(());
        };
        self.guest_buffers[index].retired = true;

        // 2. Drain.
        for blt in &self.present_blits {
            if blt.destination_resource_id != guest || blt.last_wire_fence_id == 0 {
                continue;
            }
            // Per fence at most `deadline::FENCE_MS`, cut to what the caller's section has
            // left (the whole drain is `deadline::DRAIN_MS`).
            let ns = u64::from(deadline::fence_wait_ms(
                crate::ddi::escape_wait::bounded_left_ms(),
            )) * 1_000_000;
            match ctrl::wait_fence(self.passive(), adapter, blt.last_wire_fence_id, ns) {
                ctrl::WaitFenceOutcome::Complete => {}
                ctrl::WaitFenceOutcome::TimedOut | ctrl::WaitFenceOutcome::Invalid => {
                    return Err(Why::DrainTimeout);
                }
            }
        }
        if self.guest_queue_marker(adapter).is_err() {
            return Err(Why::DrainTimeout);
        }

        // 3. The cached commands into it (several sources may share it).
        let mut i = 0;
        while i < self.present_blits.len() {
            if self.present_blits[i].destination_resource_id != guest {
                i += 1;
                continue;
            }
            let blt = self.present_blits.swap_remove(i);
            if let Err((blt, _)) = blt.release(self, adapter) {
                self.present_blits.push(blt);
                return Err(Why::ReleaseFailed);
            }
        }

        // 4. + 5. The buffer, the memory, and a fence after the free.
        let record = self.guest_buffers[index];
        if self
            .destroy_buffer_on_ring(adapter, record.buffer_id)
            .is_err()
            || self.free_memory_object(adapter, record.memory_id).is_err()
            || self.guest_queue_marker(adapter).is_err()
        {
            return Err(Why::ReleaseFailed);
        }
        self.guest_buffers.swap_remove(index);
        Ok(())
    }

    /// Record one reusable full-surface copy from an imported OPTIMAL source (through a
    /// conversion image when the formats differ) into a guest buffer. Unlike the copy into a
    /// KMD standard buffer there is no queue-family transfer on the destination (a plain,
    /// exclusive buffer only this device touches); the copy ends with the host's barrier
    /// TRANSFER/TRANSFER_WRITE -> HOST/HOST_READ, so CPU reads after the fence see it.
    pub(super) fn record_reusable_guest_blt(
        &mut self,
        adapter: &AdapterContext,
        source_image_id: VkImageId,
        conversion_image_id: Option<VkImageId>,
        destination_buffer_id: VkBufferId,
        destination_size: u64,
        width: u32,
        height: u32,
        pitch: u32,
        bytes_per_pixel: u32,
    ) -> Result<(VkCommandPoolId, VkCommandBufferId), VirtioError> {
        if destination_size == 0
            || width == 0
            || height == 0
            || bytes_per_pixel == 0
            || pitch < width.saturating_mul(bytes_per_pixel)
            || pitch % bytes_per_pixel != 0
            || conversion_image_id == Some(source_image_id)
        {
            return Err(VirtioError::DeviceError);
        }
        self.record_reusable(adapter, |s, command_buffer_id| {
            s.cmd_acquire_image_from_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            let copy_from = match conversion_image_id {
                Some(conversion_image_id) => {
                    s.cmd_internal_image_barrier(
                        adapter,
                        command_buffer_id,
                        conversion_image_id,
                        ACCESS_TRANSFER_READ | ACCESS_TRANSFER_WRITE,
                        ACCESS_TRANSFER_WRITE,
                    )?;
                    s.cmd_blit_image(
                        adapter,
                        command_buffer_id,
                        source_image_id,
                        conversion_image_id,
                        width,
                        height,
                    )?;
                    s.cmd_internal_image_barrier(
                        adapter,
                        command_buffer_id,
                        conversion_image_id,
                        ACCESS_TRANSFER_WRITE,
                        ACCESS_TRANSFER_READ,
                    )?;
                    conversion_image_id
                }
                None => source_image_id,
            };
            s.cmd_copy_image_to_buffer(
                adapter,
                command_buffer_id,
                copy_from,
                destination_buffer_id,
                width,
                height,
                pitch,
                bytes_per_pixel,
            )?;
            s.cmd_release_image_to_external(
                adapter,
                command_buffer_id,
                source_image_id,
                TransferAccess::Read,
            )?;
            s.cmd_buffer_barrier(
                adapter,
                command_buffer_id,
                BufferBarrier::transfer_write_to_host_read(destination_buffer_id, destination_size),
            )?;
            Ok(())
        })
    }
}
