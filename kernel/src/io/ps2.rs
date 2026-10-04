use ps2::Controller;
use ps2::error::ControllerError;
use ps2::flags::ControllerConfigFlags;
use spin::{Lazy, Mutex};
// use crate::{println, serial_println};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Ps2InterruptCause {
    Mouse,
    Keyboard,
}

pub struct Ps2Controller {
    controller: Controller,
}

impl Ps2Controller {
    pub fn new() -> Result<Ps2Controller, ControllerError> {
        let mut controller = unsafe { Controller::new() };

        // Step 3: Disable devices
        controller.disable_keyboard()?;
        controller.disable_mouse()?;

        // Step 4: Flush data buffer
        let _ = controller.read_data();

        // Step 5: Set config
        let mut config = controller.read_config()?;
        // Disable interrupts initially, but ENABLE translation (Set 2 -> Set 1)
        config.set(
            ControllerConfigFlags::ENABLE_KEYBOARD_INTERRUPT
                | ControllerConfigFlags::ENABLE_MOUSE_INTERRUPT,
            false,
        );
        // Enable translation so we get Scancode Set 1
        config.set(ControllerConfigFlags::ENABLE_TRANSLATE, true);
        controller.write_config(config)?;

        // Step 6: Controller self-test
        controller.test_controller()?;
        // Write config again in case of controller reset
        controller.write_config(config)?;

        // Step 7: Determine if there are 2 devices
        let has_mouse = if config.contains(ControllerConfigFlags::DISABLE_MOUSE) {
            controller.enable_mouse()?;
            config = controller.read_config()?;
            // If mouse is working, this should now be unset
            !config.contains(ControllerConfigFlags::DISABLE_MOUSE)
        } else {
            false
        };
        // Disable mouse. If there's no mouse, this is ignored
        controller.disable_mouse()?;

        // Step 8: Interface tests
        let keyboard_works = controller.test_keyboard().is_ok();
        let mouse_works = has_mouse && controller.test_mouse().is_ok();

        // Step 9 - 10: Enable and reset devices
        config = controller.read_config()?;
        if keyboard_works {
            controller.enable_keyboard()?;
            config.set(ControllerConfigFlags::DISABLE_KEYBOARD, false);
            config.set(ControllerConfigFlags::ENABLE_KEYBOARD_INTERRUPT, true);
            controller.keyboard().reset_and_self_test().unwrap();
        }
        if mouse_works {
            controller.enable_mouse()?;
            config.set(ControllerConfigFlags::DISABLE_MOUSE, false);
            config.set(ControllerConfigFlags::ENABLE_MOUSE_INTERRUPT, true);
            controller.mouse().reset_and_self_test().unwrap();
            // This will start streaming events from the mouse
            controller.mouse().enable_data_reporting().unwrap();
        }

        // Write last configuration to enable devices and interrupts
        controller.write_config(config)?;

        Ok(Ps2Controller { controller })
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    pub fn controller_mut(&mut self) -> &mut Controller {
        &mut self.controller
    }

    pub fn disable_keyboard_mouse(&mut self) -> Result<(), ControllerError> {
        self.controller.disable_keyboard()?;
        self.controller.disable_mouse()?;
        Ok(())
    }

    pub fn enable_keyboard_mouse(&mut self) -> Result<(), ControllerError> {
        self.controller.enable_keyboard()?;
        self.controller.enable_mouse()?;
        Ok(())
    }

    pub fn interrupt_cause(&mut self) -> Ps2InterruptCause {
        let status = self.controller.read_status();
        if (status.bits() & (1 << 5)) != 0 {
            Ps2InterruptCause::Mouse
        } else {
            Ps2InterruptCause::Keyboard
        }
    }

    pub fn output_has_data(&mut self) -> bool {
        let status = self.controller.read_status();
        (status.bits() & 0b1) != 0
    }
}

pub static PS2_CONTROLLER: Lazy<Mutex<Ps2Controller>> =
    Lazy::new(|| Mutex::new(Ps2Controller::new().expect("Error while Ps2Controller init")));

pub fn ps2_controller_init() {
    PS2_CONTROLLER.lock();
}
