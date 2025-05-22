use spin::{Mutex,MutexGuard};

pub struct IMutex<T>{
    mutex:Mutex<T>,
}

impl<T> IMutex<T>{
    pub const fn new(data:T)->Self{
        Self{
            mutex:Mutex::new(data),
        }
    }

    pub fn lock(&self)-> IMutexGuard<T>{
        let are_interrupts = x86_64::instructions::interrupts::are_enabled();
        if are_interrupts{
            x86_64::instructions::interrupts::disable();
        }
        IMutexGuard::new(self.mutex.lock(), are_interrupts)
    }
}

pub struct IMutexGuard<'a,T>{
    guard:MutexGuard<'a,T>,
    should_enable:bool,
}

impl<'a,T> Drop for IMutexGuard<'a,T>{
    fn drop(&mut self){
        if self.should_enable{
            x86_64::instructions::interrupts::enable();
        }
    }
}

impl<'a,T> IMutexGuard<'a,T>{
    pub fn new(guard:MutexGuard<'a,T>,should_enable:bool)->Self{
        Self{
            guard,
            should_enable,
        }
    }

    pub fn get(&self)->&T{
        &self.guard
    }

    pub fn get_mut(&mut self)->&mut T{
        &mut self.guard
    }
}

impl<'a,T> core::ops::Deref for IMutexGuard<'a,T>{
    type Target = T;

    fn deref(&self)->&Self::Target{
        &self.guard
    }
}

impl<'a,T> core::ops::DerefMut for IMutexGuard<'a,T>{
    fn deref_mut(&mut self)->&mut Self::Target{
        &mut self.guard
    }
}