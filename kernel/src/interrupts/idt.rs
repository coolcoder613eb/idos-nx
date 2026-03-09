use super::exceptions;
use super::ipi;
use super::stack::StackFrame;
use crate::arch::segment::SegmentSelector;
use core::arch::asm;

// Flags used in IDT entries
pub const IDT_PRESENT: u8 = 1 << 7;
pub const IDT_DESCRIPTOR_RING_0: u8 = 0;
pub const IDT_DESCRIPTOR_RING_3: u8 = 3 << 5;
pub const IDT_GATE_TYPE_INT_32: u8 = 0xe;

pub type HandlerFunction = unsafe extern "x86-interrupt" fn(StackFrame);
pub type HandlerFunctionWithError = unsafe extern "x86-interrupt" fn(StackFrame, u32);

extern "x86-interrupt" {
    fn pic_irq_0(frame: StackFrame) -> ();
    fn pic_irq_1(frame: StackFrame) -> ();
    fn pic_irq_3(frame: StackFrame) -> ();
    fn pic_irq_4(frame: StackFrame) -> ();
    fn pic_irq_5(frame: StackFrame) -> ();
    fn pic_irq_6(frame: StackFrame) -> ();
    fn pic_irq_7(frame: StackFrame) -> ();
    fn pic_irq_8(frame: StackFrame) -> ();
    fn pic_irq_9(frame: StackFrame) -> ();
    fn pic_irq_a(frame: StackFrame) -> ();
    fn pic_irq_b(frame: StackFrame) -> ();
    fn pic_irq_c(frame: StackFrame) -> ();
    fn pic_irq_d(frame: StackFrame) -> ();
    fn pic_irq_e(frame: StackFrame) -> ();
    fn pic_irq_f(frame: StackFrame) -> ();

    fn syscall_handler(frame: StackFrame) -> ();
    fn gpf_exception(frame: StackFrame, error: u32) -> ();
    fn debug_exception(frame: StackFrame) -> ();
}

/// An IDT Entry tells the x86 CPU how to handle an interrupt.
/// The entry attributes determine how the interrupt is entered, what permission
/// ring and memory selector to use, and which address to enter.
#[derive(Copy, Clone)]
#[repr(C, packed)]
pub struct IdtEntry {
    pub offset_low: u16,
    pub selector: SegmentSelector,
    pub zero: u8,
    pub type_and_attributes: u8,
    pub offset_high: u16,
}

impl IdtEntry {
    pub const fn new() -> Self {
        Self {
            offset_low: 0,
            selector: SegmentSelector::new(1, 0),
            zero: 0,
            type_and_attributes: 0,
            offset_high: 0,
        }
    }

    /// Set the handler function for this entry. When this interrupt occurs,
    /// the CPU will attempt to enter this function.
    pub fn set_handler(&mut self, func: HandlerFunction) {
        let offset = func as *const () as usize;
        self.set_handler_at_offset(offset);
    }

    pub fn set_handler_with_error(&mut self, func: HandlerFunctionWithError) {
        let offset = func as *const () as usize;
        self.set_handler_at_offset(offset);
    }

    /// The actual implementation for setting the handler data
    fn set_handler_at_offset(&mut self, offset: usize) {
        self.offset_low = offset as u16;
        self.offset_high = (offset >> 16) as u16;
        self.type_and_attributes = IDT_PRESENT | IDT_GATE_TYPE_INT_32;
    }

    /// Allow the interrupt to be called from Ring 3. This is required for any
    /// syscalls.
    pub fn make_usermode_accessible(&mut self) {
        self.type_and_attributes |= IDT_DESCRIPTOR_RING_3;
    }
}

/// The IDT Descriptor is a special in-memory data structure that tells the CPU
/// how to find the actual IDT table. Because the CPU needs to know how many
/// valid entries exist in the table, it requires this extra layer of
/// indirection.
#[repr(C, packed)]
pub struct IdtDescriptor {
    pub size: u16,
    pub offset: u32,
}

impl IdtDescriptor {
    pub const fn new() -> Self {
        Self { size: 0, offset: 0 }
    }

    pub fn point_to(&mut self, idt: &[IdtEntry]) {
        self.size = (idt.len() * core::mem::size_of::<IdtEntry>() - 1) as u16;
        self.offset = &idt[0] as *const IdtEntry as u32;
    }

    pub fn load(&self) {
        unsafe {
            asm!(
                "lidt [{desc}]",
                desc = in(reg) self,
                options(preserves_flags, nostack),
            );
        }
    }
}

// Global tables and structures:

pub static mut IDTR: IdtDescriptor = IdtDescriptor::new();

pub static mut IDT: [IdtEntry; 256] = [IdtEntry::new(); 256];

pub unsafe fn init_idt() {
    IDTR.point_to(&IDT);

    // Set exception handlers. Because all interrupt handlers are currently
    // hard-coded to be Interrupt Gate types (vs Task), they will disable other
    // interrupts when triggered. If we make the kernel interrupt-safe, these
    // can be updated to tasks and made interruptable themselves.
    IDT[0x00].set_handler(exceptions::div);
    IDT[0x01].set_handler(debug_exception);
    IDT[0x02].set_handler(exceptions::nmi);
    IDT[0x03].set_handler(exceptions::breakpoint);
    IDT[0x04].set_handler(exceptions::overflow);
    IDT[0x05].set_handler(exceptions::bound_exceeded);
    IDT[0x06].set_handler(exceptions::invalid_opcode);
    IDT[0x07].set_handler(exceptions::fpu_not_available);
    IDT[0x08].set_handler_with_error(exceptions::double_fault);
    // IDT entry 9 is no longer valid
    IDT[0x0a].set_handler_with_error(exceptions::invalid_tss);
    IDT[0x0b].set_handler_with_error(exceptions::segment_not_present);
    IDT[0x0c].set_handler_with_error(exceptions::stack_segment_fault);
    IDT[0x0d].set_handler_with_error(gpf_exception);
    IDT[0x0e].set_handler_with_error(exceptions::page_fault);

    // Interrupts through 0x1f represent exceptions that we don't handle,
    // usually because they are deprecated or represent unsupported hardware.

    // Interrupts 0x20-0x2f are mostly unused, to avoid conflict with legacy
    // DOS interrupts. The only one used by the kernel is 0x2b, which is the
    // entrypoint for user-mode programs to make a syscall.

    IDT[0x2b].set_handler(syscall_handler);
    IDT[0x2b].make_usermode_accessible();

    // Interrupts 0x30-0x3f are reserved for PIC hardware interrupts.
    // This is where we begin to allow processes to install their own interrupt
    // handlers. For example, a COM driver would want to listen to interrupt
    // 0x34. To accommodate this, these interrupts have a handler that runs
    // through a vector of installed hooks before returning to whatever code
    // was originally running before the interrupt.

    // IRQ 0 is always the PIT timer chip
    IDT[0x30].set_handler(pic_irq_0);
    // IRQ 1 is the keyboard PS/2 controller
    IDT[0x31].set_handler(pic_irq_1);
    // IRQ 2 does not exist, since it is the cascade for the secondary PIC
    // The remaining IRQs are a mix of standard connections and ISA interrupts.
    // When PCI devices are available, their interrupts are exposed on unused
    // lines.
    IDT[0x33].set_handler(pic_irq_3);
    IDT[0x34].set_handler(pic_irq_4);
    IDT[0x35].set_handler(pic_irq_5);
    IDT[0x36].set_handler(pic_irq_6);
    IDT[0x37].set_handler(pic_irq_7);
    IDT[0x38].set_handler(pic_irq_8);
    IDT[0x39].set_handler(pic_irq_9);
    IDT[0x3a].set_handler(pic_irq_a);
    IDT[0x3b].set_handler(pic_irq_b);
    IDT[0x3c].set_handler(pic_irq_c);
    IDT[0x3d].set_handler(pic_irq_d);
    IDT[0x3e].set_handler(pic_irq_e);
    IDT[0x3f].set_handler(pic_irq_f);

    // Inter-process interrupts are sent to the top vectors
    IDT[0xf0].set_handler(ipi::pit_cascade);

    IDTR.load();
}
