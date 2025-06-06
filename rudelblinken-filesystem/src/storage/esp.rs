/// Storage implementation backed by esp32-c3 flash
// TODO: Write better module level docs
use crate::{
    storage::{EraseStorageError, Storage, StorageError},
    Filesystem,
};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_sys::{
    esp_err_to_name, esp_partition_erase_range, esp_partition_find, esp_partition_get,
    esp_partition_mmap, esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA, esp_partition_next,
    esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
    esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED,
    esp_partition_type_t_ESP_PARTITION_TYPE_ANY, esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
    esp_partition_write_raw, ESP_OK,
};
use std::{
    os::raw::c_void,
    sync::{Mutex, RwLock},
};
use thiserror::Error;

/// A storage implementation that stores data in the flash of the ESP32-C3
pub struct FlashStorage {
    partition: *const esp_idf_sys::esp_partition_t,
    nvs: Mutex<EspNvs<NvsDefault>>,

    storage_arena: *mut u8,
}

unsafe impl Sync for FlashStorage {}
unsafe impl Send for FlashStorage {}

/// Log information about the available partitions
pub fn print_partitions() {
    unsafe {
        let mut partition_iterator = esp_partition_find(
            esp_partition_type_t_ESP_PARTITION_TYPE_ANY,
            esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
            std::ptr::null_mut(),
        );
        if partition_iterator == std::ptr::null_mut() {
            panic!("No partitions found!");
        }
        // println!("type, subtype, label, address, name");

        while partition_iterator != std::ptr::null_mut() {
            let partition = *esp_partition_get(partition_iterator);
            let label = String::from_utf8(std::mem::transmute(partition.label.to_vec()));
            // label.copy_from_slice(&partition.label.);
            println!(
                "{}, {}, {:?}, {:0x}, {}",
                partition.type_, partition.subtype, label, partition.address, partition.size
            );
            partition_iterator = esp_partition_next(partition_iterator);
        }
    }
}

#[derive(Error, Debug, Clone)]
/// An error while opening an esp32 storage
pub enum CreateStorageError {
    /// Failed to find a storage partition. (type: data, subtype: undefined, name: storage)
    #[error("Failed to find a storage partition. (type: data, subtype: undefined, name: storage)")]
    NoPartitionFound,
    /// Failed to memorymap the secrets
    #[error("Failed to memorymap the secrets")]
    FailedToMmapSecrets,
    /// Failed to find the default nvs partition
    #[error("Failed to find the default nvs partition")]
    NoNvsPartitionFound,
    /// Failed to open filesystem1 nvs namespace
    #[error("Failed to open filesystem1 nvs namespace")]
    FailedToOpenNvsNamespace,
    /// The erase size of the underlying flash does not match the static block size
    #[error("The erase size of the underlying flash does not match the static block size")]
    EraseSizeDoesNotMatchBlockSize,
}

impl FlashStorage {
    /// Find the partition named storage and load a filesystem from it.
    ///
    /// Note that this is only safe if nothing else is writing to that storage until the device is reset
    pub fn new() -> Result<FlashStorage, CreateStorageError> {
        // TODO: Make sure that there is only one flash storage instance.
        let mut label: Vec<i8> = String::from("storage")
            .bytes()
            .into_iter()
            .map(|c| c as i8)
            .collect();
        label.push(0);

        // Find the partition
        let partition;
        unsafe {
            let partition_iterator = esp_partition_find(
                esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED,
                label.as_mut_ptr(),
            );
            if partition_iterator == std::ptr::null_mut() {
                return Err(CreateStorageError::NoPartitionFound);
            }
            partition = esp_partition_get(partition_iterator);
            if (*partition).erase_size as u32 != Self::BLOCK_SIZE {
                return Err(CreateStorageError::EraseSizeDoesNotMatchBlockSize);
            }
        }

        // Memorymap the partition
        let memory_mapped_flash: *mut u8;
        let mut storage_handle_a: u32 = 0;
        let mut storage_handle_b: u32 = 0;
        let mut storage_handle_c: u32 = 0;
        unsafe {
            let mut first_pointer: *const c_void = std::ptr::null_mut();
            // Mount first mmu page
            let err = esp_partition_mmap(
                partition,
                0,
                esp_idf_sys::CONFIG_MMU_PAGE_SIZE as usize,
                esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                std::ptr::addr_of_mut!(first_pointer),
                std::ptr::addr_of_mut!(storage_handle_a),
            );
            if err != 0 {
                return Err(CreateStorageError::FailedToMmapSecrets);
            }
            let mut idk_pointer: *const c_void = std::ptr::null_mut();
            // Mount the remaining pages
            let err = esp_partition_mmap(
                partition,
                esp_idf_sys::CONFIG_MMU_PAGE_SIZE as usize,
                (*partition).size as usize - esp_idf_sys::CONFIG_MMU_PAGE_SIZE as usize,
                esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                std::ptr::addr_of_mut!(idk_pointer),
                std::ptr::addr_of_mut!(storage_handle_b),
            );
            if err != 0 {
                return Err(CreateStorageError::FailedToMmapSecrets);
            }
            // If we now mmap the whole partition, will get a pointer to the memory mapped partition directly after the first a partition.
            // If we would have mounted the whole partition in one step previously, we would have got the same pointer again
            let err = esp_partition_mmap(
                partition,
                0,
                (*partition).size as usize,
                esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                std::ptr::addr_of_mut!(idk_pointer),
                std::ptr::addr_of_mut!(storage_handle_c),
            );
            if err != 0 {
                // println!("Errorcode: {}", err);
                // let error: &std::ffi::CStr = std::ffi::CStr::from_ptr(esp_err_to_name(err));
                // println!("Error description: {}", error.to_string_lossy());
                return Err(CreateStorageError::FailedToMmapSecrets);
            }

            // println!("Got out_ptr: {:0x?}", first_pointer);
            memory_mapped_flash = first_pointer as _;

            let nvs_default_partition: EspNvsPartition<NvsDefault> =
                EspDefaultNvsPartition::take().or(Err(CreateStorageError::NoNvsPartitionFound))?;
            let nvs = EspNvs::new(nvs_default_partition, "filesystem1", true)
                .or(Err(CreateStorageError::FailedToOpenNvsNamespace))?;

            return Ok(FlashStorage {
                partition: partition,
                nvs: Mutex::new(nvs),

                // size: (*partition).size as usize,
                storage_arena: memory_mapped_flash,
                // storage_handle_a,
                // storage_handle_b,
                // storage_handle_c,
            });
        }
    }
}

impl Storage for FlashStorage {
    const BLOCKS: u32 = 256;
    const BLOCK_SIZE: u32 = 4096;

    fn read(&self, address: u32, length: u32) -> Result<&'static [u8], StorageError> {
        // TODO: Make this actually safe
        if (address) > Self::BLOCKS * Self::BLOCK_SIZE {
            return Err(StorageError::AddressTooBig.into());
        }
        if (address + length) > Self::BLOCKS * Self::BLOCK_SIZE * 2 {
            // TODO: Support erase with wraparound
            return Err(StorageError::SizeTooBig.into());
        }
        let thing: &[u8];
        unsafe {
            // ::log::info!("Reading {} bytes at {}", length, address);
            thing = std::slice::from_raw_parts(
                self.storage_arena.offset(address as isize),
                length as usize,
            );
            // ::log::info!("Read data");
        }
        return Ok(thing);
    }

    fn write(&self, address: u32, data: &[u8]) -> Result<(), StorageError> {
        // TODO: Make this actually safe
        let data_ptr = data.as_ptr() as *const c_void;
        // println!(
        //     "STORAGE: {:0x?}, INPUT: {:0x?}",
        //     self.storage_arena, data_ptr
        // );
        unsafe {
            // Works with erase
            // esp_partition_erase_range(self.partition, 0, (*self.partition).erase_size as usize);

            let error_code =
                esp_partition_write_raw(self.partition, address as usize, data_ptr, data.len());
            if error_code != ESP_OK {
                // println!("Failed to write to flash with code {}", error_code);
                let error: &std::ffi::CStr = std::ffi::CStr::from_ptr(esp_err_to_name(error_code));
                // println!("Description: {}", error.to_string_lossy());
                return Err(StorageError::Other(error.to_string_lossy().into()));
            }
        };
        // unsafe {
        //     std::ptr::copy_nonoverlapping(data_ptr, self.storage_arena, data.len());
        // }
        // println!("Copied data");
        return Ok(());
    }

    fn erase(&self, address: u32, length: u32) -> Result<(), EraseStorageError> {
        if length == 0 {
            return Ok(());
        }
        if address % Self::BLOCK_SIZE != 0 {
            return Err(EraseStorageError::CanOnlyEraseAlongBlockBoundaries);
        }
        if length % Self::BLOCK_SIZE != 0 {
            return Err(EraseStorageError::CanOnlyEraseInBlockSizedChunks);
        }
        if (address) > Self::BLOCKS * Self::BLOCK_SIZE {
            return Err(StorageError::AddressTooBig.into());
        }
        if (address + length) > Self::BLOCKS * Self::BLOCK_SIZE {
            // TODO: Support erase with wraparound
            return Err(StorageError::SizeTooBig.into());
        }

        unsafe {
            // println!(
            //     "Erasing {} blocks starting from {}",
            //     length / Self::BLOCK_SIZE,
            //     address / Self::BLOCK_SIZE
            // );
            let error_code =
                esp_partition_erase_range(self.partition, address as usize, length as usize);
            if error_code != ESP_OK {
                // println!("Failed to erase flash with code {}", error_code);
                let error: &std::ffi::CStr = std::ffi::CStr::from_ptr(esp_err_to_name(error_code));
                // println!("Description: {}", error.to_string_lossy());
                return Err(StorageError::Other(error.to_string_lossy().into()).into());
            }
        }
        return Ok(());
    }

    fn read_metadata(&self, key: &str) -> std::io::Result<Box<[u8]>> {
        let mut read_buffer = [0u8; 256];
        let buffer = self
            .nvs
            .lock()
            .map_err(|_| std::io::Error::other("Failed to obtain lock to nvs"))?
            .get_raw(key, &mut read_buffer)
            .map_err(|_| std::io::Error::other("Failed to read value from nvs"))?
            .ok_or(std::io::ErrorKind::NotFound)?;
        let boxed_result: Box<[u8]> = buffer.iter().cloned().collect();
        return Ok(boxed_result);
    }

    fn write_metadata(&self, key: &str, value: &[u8]) -> std::io::Result<()> {
        self.nvs
            .lock()
            .map_err(|_| std::io::Error::other("Failed to obtain lock to nvs"))?
            .set_raw(key, value)
            .map_err(|_| std::io::Error::other("Failed to write value to nvs"))?;
        return Ok(());
    }
}

static mut STORAGE_SINGLETON: Option<FlashStorage> = None;
static mut FILESYSTEM_SINGLETON: Option<RwLock<Filesystem<FlashStorage>>> = None;

/// An error occurred while initializing the global storage singleton
#[derive(Error, Debug, Clone)]
pub enum SetupStorageError {
    /// Storage is already initialized
    #[error("Storage is already initialized.")]
    AlreadyInitialized,
    /// An error while creating the storage
    #[error(transparent)]
    CreateStorageError(#[from] CreateStorageError),
}

/// Setup the global storage singleton
fn setup_storage() -> Result<(), SetupStorageError> {
    unsafe {
        STORAGE_SINGLETON = Some(FlashStorage::new()?);
        FILESYSTEM_SINGLETON = Some(RwLock::new(Filesystem::new(
            STORAGE_SINGLETON.as_ref().unwrap(),
        )));
        dbg!(&FILESYSTEM_SINGLETON.as_ref().unwrap().read().unwrap().files);
    }
    return Ok(());
}

/// Get the global filesystem singleton
fn get_filesystem() -> Result<&'static RwLock<Filesystem<FlashStorage>>, SetupStorageError> {
    unsafe {
        if FILESYSTEM_SINGLETON.is_none() {
            setup_storage()?;
        }
        Ok(FILESYSTEM_SINGLETON.as_ref().unwrap())
    }
}

// fn get_first_block() -> u16 {
//     let nvs_default_partition = EspDefaultNvsPartition::take().unwrap();
//     let Ok(nvs) = EspNvs::new(nvs_default_partition, "filesystem_ns", false) else {
//         panic!("Something went wrong");
//     };
//     nvs.get_u16("first_block").unwrap_or(Some(0)).unwrap_or(0)
// }

// fn set_first_block(first_block: u16) {
//     let nvs_default_partition = EspDefaultNvsPartition::take().unwrap();
//     let Ok(nvs) = EspNvs::new(nvs_default_partition, "filesystem_ns", true) else {
//         panic!("Something went wrong");
//     };
//     nvs.set_u16("first_block", first_block).unwrap();
// }

// struct Filesystem<T: Storage> {
//     storage: T,
//     files: Vec<File>,
// }

// impl<T: Storage> Filesystem<T> {
//     pub fn new(storage: T) -> Self {
//         let first_block = get_first_block() as usize;

//         let mut files = Vec::new();
//         let mut block_number = 0;
//         while block_number < T::BLOCKS {
//             let current_block_number = (block_number + first_block as usize) % T::BLOCKS;
//             let file_information = File::new(&storage, current_block_number * T::BLOCK_SIZE);
//             let Ok(file_information) = file_information else {
//                 block_number += 1;
//                 continue;
//             };
//             block_number += ((file_information.length + 64) / T::BLOCK_SIZE) + 1;
//             files.push(file_information);
//         }

//         let filesystem = Self {
//             storage,
//             files: Vec::new(),
//         };
//         return filesystem;
//     }
// }
// fn init() {
//     // example storage backend
//     ram_storage!(tiny);
//     let mut ram = Ram::default();
//     let mut storage = RamStorage::new(&mut ram);

//     // must format before first mount
//     Filesystem::format(&mut storage).unwrap();
//     // must allocate state statically before use
//     let mut alloc = Filesystem::allocate();
//     let mut fs = Filesystem::mount(&mut alloc, &mut storage).unwrap();

//     // may use common `OpenOptions`
//     let mut buf = [0u8; 11];
//     fs.open_file_with_options_and_then(
//         |options| options.read(true).write(true).create(true),
//         &PathBuf::from(b"example.txt"),
//         |file| {
//             file.write(b"Why is black smoke coming out?!")?;
//             file.seek(SeekFrom::End(-24)).unwrap();
//             assert_eq!(file.read(&mut buf)?, 11);
//             Ok(())
//         },
//     )
//     .unwrap();
//     assert_eq!(&buf, b"black smoke");
// }
