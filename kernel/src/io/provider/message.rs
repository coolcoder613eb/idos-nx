use super::{AsyncOpQueue, IOProvider, OpIdGenerator, UnmappedAsyncOp};
use crate::{
    io::{async_io::AsyncOpID, handle::Handle},
    memory::{
        address::{PhysicalAddress, VirtualAddress},
        virt::scratch::UnmappedPage,
    },
    task::{
        id::TaskID, map::get_task, messaging::MessagePacket, paging::get_current_physical_address,
        switching::get_current_id,
    },
};
use alloc::collections::BTreeMap;
use idos_api::io::error::IoResult;
use idos_api::io::AsyncOp;
use idos_api::ipc::Message;
use spin::RwLock;

/// Inner contents of the handle used to read IPC messages.
pub struct MessageIOProvider {
    task_id: TaskID,
    id_gen: OpIdGenerator,
    pending_ops: RwLock<BTreeMap<AsyncOpID, UnmappedAsyncOp>>,
}

impl MessageIOProvider {
    pub fn for_task(task_id: TaskID) -> Self {
        Self {
            task_id,
            id_gen: OpIdGenerator::new(),
            pending_ops: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn pop_message(&self) -> Option<MessagePacket> {
        let current_ticks = 0;
        let task_lock = get_task(self.task_id)?;
        let (first_message, _has_more) = {
            let mut task_guard = task_lock.write();
            task_guard.message_queue.read(current_ticks)
        };
        first_message
    }

    pub fn check_messages(&self) {
        if self.pending_ops.read().is_empty() {
            // if there are no pending operations, avoid popping the recently
            // sent message
            return;
        }
        loop {
            let packet = match self.pop_message() {
                Some(packet) => packet,
                None => return,
            };
            let (sender, message) = packet.open();
            if let Some(first) = self.pending_ops.write().pop_first() {
                let (id, op) = first;
                let message_paddr = op.args[0];
                Self::copy_message(message_paddr, message);
                op.complete(sender.into());
            }
        }
    }

    pub fn copy_message(message_paddr: u32, message: Message) {
        let phys_frame_start = message_paddr & 0xfffff000;
        let unmapped_phys = PhysicalAddress::new(phys_frame_start);
        let unmapped_page = UnmappedPage::map(unmapped_phys);
        let message_offset = message_paddr & 0xfff;
        unsafe {
            let ptr = (unmapped_page.virtual_address() + message_offset).as_ptr_mut::<Message>();
            core::ptr::write_volatile(ptr, message);
        }
    }
}

impl IOProvider for MessageIOProvider {
    fn add_op(
        &self,
        provider_index: u32,
        op: &AsyncOp,
        args: [u32; 3],
        wake_set: Option<Handle>,
    ) -> AsyncOpID {
        // convert the virtual address of the message pointer to a physical
        // address
        // TODO: if the message spans two physical pages, we're gonna have a problem!
        let message_size = core::mem::size_of::<Message>() as u32;
        if (args[0] & 0xfffff000) != ((args[0] + message_size) & 0xfffff000) {
            panic!("Messages can't bridge multiple pages (yet)");
        }
        let message_virt = VirtualAddress::new(args[0]);
        let message_phys = get_current_physical_address(message_virt)
            .expect("Tried to reference unmapped address");

        let id = self.id_gen.next_id();
        let mut unmapped =
            UnmappedAsyncOp::from_op(op, args, wake_set.map(|handle| (get_current_id(), handle)));
        unmapped.args[0] = message_phys.as_u32();

        self.pending_ops.write().insert(id, unmapped);

        match self.run_op(provider_index, id) {
            Some(result) => {
                let unmapped = self.remove_op(id);
                let return_value = match result {
                    Ok(inner) => inner & 0x7fffffff,
                    Err(inner) => Into::<u32>::into(inner) | 0x80000000,
                };
                if let Some(unmapped) = unmapped {
                    unmapped.complete(return_value);
                }
            }
            None => (),
        }
        id
    }

    fn get_op(&self, id: AsyncOpID) -> Option<UnmappedAsyncOp> {
        self.pending_ops.read().get(&id).cloned()
    }

    fn remove_op(&self, id: AsyncOpID) -> Option<UnmappedAsyncOp> {
        self.pending_ops.write().remove(&id)
    }

    fn read(&self, _provider_index: u32, _id: AsyncOpID, op: UnmappedAsyncOp) -> Option<IoResult> {
        let packet = self.pop_message()?;
        let (sender, message) = packet.open();
        Self::copy_message(op.args[0], message);
        Some(Ok(sender.into()))
    }
}
