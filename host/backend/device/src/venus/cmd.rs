//! Display info, the EDID, capsets, contexts and command submission.

use super::*;

impl Venus {
    /// One scanout, the configured display's size; enabled iff there is a
    /// display. The other fifteen are off.
    pub(super) fn display_info(&self) -> Reply {
        let mut info = RespDisplayInfo::default();
        if let Some(d) = self.display {
            info.pmodes[0] = DisplayOne {
                r: Rect {
                    x: 0,
                    y: 0,
                    width: d.width,
                    height: d.height,
                },
                enabled: 1,
                flags: 0,
            };
        }
        Reply::With(
            RESP_OK_DISPLAY_INFO,
            info.to_bytes()[CTRL_HDR_LEN..].to_vec(),
        )
    }

    /// Scanout 0's EDID (`edid.rs`): the configured display's size and
    /// refresh, in an EDID 1.4 block and a DisplayID 2.0 extension. Another
    /// scanout is `RESP_ERR_INVALID_SCANOUT_ID`; without a display there is
    /// no monitor to describe, and the answer is `RESP_ERR_UNSPEC` (the
    /// guest then uses its own EDID, as on any error).
    pub(super) fn edid(&self, c: &GetEdid) -> Answer {
        if c.scanout != 0 {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        let Some(d) = self.display else {
            return Err(RESP_ERR_UNSPEC);
        };
        let bytes = edid::edid(d.width, d.height, d.refresh_hz);
        let mut r = RespEdid {
            size: bytes.len() as u32,
            ..Default::default()
        };
        r.edid[..bytes.len()].copy_from_slice(&bytes);
        Ok(Reply::With(
            RESP_OK_EDID,
            r.to_bytes()[CTRL_HDR_LEN..].to_vec(),
        ))
    }

    /// Index 0 is Venus; there is no other.
    pub(super) fn capset_info(&mut self, c: &GetCapsetInfo) -> Answer {
        if c.capset_index != 0 {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let info = self
            .renderer
            .capset_info(0)
            .map_err(|e| self.renderer_error(&e))?;
        if info.id != CAPSET_VENUS {
            log::warn!("venus: the renderer's capset 0 is {}, not Venus", info.id);
            return Err(RESP_ERR_UNSPEC);
        }
        let r = RespCapsetInfo {
            capset_id: info.id,
            capset_max_version: info.max_version,
            capset_max_size: info.max_size,
            ..Default::default()
        };
        Ok(Reply::With(
            RESP_OK_CAPSET_INFO,
            r.to_bytes()[CTRL_HDR_LEN..].to_vec(),
        ))
    }

    pub(super) fn capset(&mut self, c: &GetCapset) -> Answer {
        if c.capset_id != CAPSET_VENUS {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let data = self
            .renderer
            .capset(c.capset_id, c.capset_version)
            .map_err(|e| self.renderer_error(&e))?;
        Ok(Reply::With(RESP_OK_CAPSET, data))
    }

    /// A context with `context_init` capset Venus, and nothing else.
    pub(super) fn ctx_create(&mut self, c: &CtxCreate) -> Answer {
        let id = c.hdr.ctx_id;
        if id == 0 || self.contexts.contains(&id) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        if c.capset_id() != CAPSET_VENUS || c.nlen > 64 {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        if self.contexts.len() >= MAX_CONTEXTS {
            log::warn!("venus: context {id} refused: {MAX_CONTEXTS} already");
            return Err(RESP_ERR_OUT_OF_MEMORY);
        }
        self.renderer
            .ctx_create(id, CAPSET_VENUS, c.name())
            .map_err(|e| self.renderer_error(&e))?;
        self.contexts.insert(id);
        Ok(Reply::NoData)
    }

    /// The context goes, and every resource attached to it is detached
    /// first. The resources themselves stay until unreferenced.
    pub(super) fn ctx_destroy(&mut self, id: u32) -> Answer {
        if !self.contexts.remove(&id) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        for (&res, r) in self.resources.iter_mut() {
            if r.attached.remove(&id) {
                self.renderer.ctx_detach(id, res);
            }
        }
        self.renderer.ctx_destroy(id);
        Ok(Reply::NoData)
    }

    pub(super) fn ctx_attach(&mut self, ctx: u32, res: u32) -> Answer {
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        let Some(r) = self.resources.get_mut(&res) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        // Already attached (the creating context always is): nothing to do,
        // and the renderer is not asked twice.
        if r.attached.contains(&ctx) {
            return Ok(Reply::NoData);
        }
        if let Err(e) = self.renderer.ctx_attach(ctx, res) {
            return Err(self.renderer_error(&e));
        }
        self.resources
            .get_mut(&res)
            .expect("looked up above")
            .attached
            .insert(ctx);
        Ok(Reply::NoData)
    }

    /// Only a resource attached to the context can be detached from it.
    pub(super) fn ctx_detach(&mut self, ctx: u32, res: u32) -> Answer {
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        let Some(r) = self.resources.get_mut(&res) else {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        };
        if !r.attached.remove(&ctx) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        self.renderer.ctx_detach(ctx, res);
        Ok(Reply::NoData)
    }

    pub(super) fn submit(&mut self, ctx: u32, commands: &[u8]) -> Answer {
        if !self.contexts.contains(&ctx) {
            return Err(RESP_ERR_INVALID_CONTEXT_ID);
        }
        self.renderer
            .submit(ctx, commands)
            .map_err(|e| self.renderer_error(&e))?;
        Ok(Reply::NoData)
    }
}
