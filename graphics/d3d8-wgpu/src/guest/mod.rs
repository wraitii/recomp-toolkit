use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;

pub(crate) mod buffer;
pub(crate) mod hresult;
pub(crate) mod texture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestError {
    InvalidAddress,
    OutOfMemory,
    InvalidAllocation,
    InvalidServices,
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for GuestError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestAddr(u32);

impl GuestAddr {
    pub fn new(value: u32) -> Result<Self, GuestError> {
        if value == 0 {
            Err(GuestError::InvalidAddress)
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn checked_add(self, offset: u32) -> Result<Self, GuestError> {
        Self::new(
            self.0
                .checked_add(offset)
                .ok_or(GuestError::InvalidAddress)?,
        )
    }
}

#[repr(C)]
pub struct GuestMemoryCallbacks {
    pub context: *mut c_void,
    pub validate: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> u32>,
    pub read: Option<unsafe extern "C" fn(*mut c_void, u32, *mut u8, u32) -> u32>,
    pub write: Option<unsafe extern "C" fn(*mut c_void, u32, *const u8, u32) -> u32>,
    pub allocate: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> u32>,
    pub free: Option<unsafe extern "C" fn(*mut c_void, u32, u64) -> u32>,
    pub generation: Option<unsafe extern "C" fn(*mut c_void) -> u64>,
    pub write_words: Option<unsafe extern "C" fn(*mut c_void, u32, *const u32, u32) -> u32>,
}

#[derive(Debug)]
pub struct GuestAllocation {
    address: GuestAddr,
    generation: u64,
}

impl GuestAllocation {
    pub fn address(&self) -> GuestAddr {
        self.address
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

pub struct GuestMemory<'a> {
    callbacks: &'a GuestMemoryCallbacks,
    _thread: PhantomData<Rc<()>>,
}

impl<'a> GuestMemory<'a> {
    /// The callbacks must remain valid and the guest baton held throughout this scope.
    /// The scope must not cross a guest callback, yield, or arena content swap.
    pub unsafe fn new(callbacks: &'a GuestMemoryCallbacks) -> Result<Self, GuestError> {
        if callbacks.validate.is_none()
            || callbacks.read.is_none()
            || callbacks.write.is_none()
            || callbacks.allocate.is_none()
            || callbacks.free.is_none()
            || callbacks.generation.is_none()
            || callbacks.write_words.is_none()
        {
            return Err(GuestError::InvalidServices);
        }
        Ok(Self {
            callbacks,
            _thread: PhantomData,
        })
    }

    pub fn validate(&self, address: GuestAddr, len: u32) -> Result<(), GuestError> {
        if len != 0 && address.0.checked_add(len - 1).is_none() {
            return Err(GuestError::InvalidAddress);
        }
        if unsafe { (self.callbacks.validate.unwrap())(self.callbacks.context, address.0, len) }
            == 0
        {
            Err(GuestError::InvalidAddress)
        } else {
            Ok(())
        }
    }

    pub fn read(&self, address: GuestAddr, out: &mut [u8]) -> Result<(), GuestError> {
        let len = u32::try_from(out.len()).map_err(|_| GuestError::InvalidAddress)?;
        self.validate(address, len)?;
        if unsafe {
            (self.callbacks.read.unwrap())(self.callbacks.context, address.0, out.as_mut_ptr(), len)
        } == 0
        {
            Err(GuestError::InvalidAddress)
        } else {
            Ok(())
        }
    }

    pub fn write(&mut self, address: GuestAddr, bytes: &[u8]) -> Result<(), GuestError> {
        let len = u32::try_from(bytes.len()).map_err(|_| GuestError::InvalidAddress)?;
        self.validate(address, len)?;
        if unsafe {
            (self.callbacks.write.unwrap())(self.callbacks.context, address.0, bytes.as_ptr(), len)
        } == 0
        {
            Err(GuestError::InvalidAddress)
        } else {
            Ok(())
        }
    }

    pub fn write_u32(&mut self, address: GuestAddr, value: u32) -> Result<(), GuestError> {
        self.write_words(address, &[value])
    }

    pub fn write_words(&mut self, address: GuestAddr, words: &[u32]) -> Result<(), GuestError> {
        let count = u32::try_from(words.len()).map_err(|_| GuestError::InvalidAddress)?;
        let len = count.checked_mul(4).ok_or(GuestError::InvalidAddress)?;
        self.validate(address, len)?;
        if unsafe {
            (self.callbacks.write_words.unwrap())(
                self.callbacks.context,
                address.0,
                words.as_ptr(),
                count,
            )
        } == 0
        {
            Err(GuestError::InvalidAddress)
        } else {
            Ok(())
        }
    }

    pub fn generation(&self) -> u64 {
        unsafe { (self.callbacks.generation.unwrap())(self.callbacks.context) }
    }

    pub fn allocate(&mut self, size: u32, align: u32) -> Result<GuestAllocation, GuestError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(GuestError::InvalidAllocation);
        }
        let generation = self.generation();
        let raw =
            unsafe { (self.callbacks.allocate.unwrap())(self.callbacks.context, size, align) };
        let address = GuestAddr::new(raw).map_err(|_| GuestError::OutOfMemory)?;
        if self.generation() != generation {
            return Err(GuestError::InvalidAllocation);
        }
        if raw % align != 0 || self.validate(address, size).is_err() {
            unsafe { (self.callbacks.free.unwrap())(self.callbacks.context, raw, generation) };
            return Err(GuestError::InvalidAllocation);
        }
        Ok(GuestAllocation {
            address,
            generation,
        })
    }

    pub fn free(&mut self, allocation: GuestAllocation) -> Result<(), GuestError> {
        if allocation.generation != self.generation() {
            return Ok(());
        }
        if unsafe {
            (self.callbacks.free.unwrap())(
                self.callbacks.context,
                allocation.address.0,
                allocation.generation,
            )
        } == 0
        {
            Err(GuestError::InvalidAllocation)
        } else {
            Ok(())
        }
    }
}
