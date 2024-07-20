#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use block_device_adapters::BufStream;
use cortex_m::asm::wfe;
use defmt::info;
use ekv::{
    config,
    flash::{self, PageID},
    Database,
};
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::{
    gpio::{Level, Output},
    peripherals::*,
    spi::{Async, Config, Spi},
};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    mutex::Mutex,
};
use embedded_hal_async::delay::DelayNs;
use embedded_storage_async::nor_flash::{NorFlash, ReadNorFlash};
use sdspi::{sd_init, SdSpi};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

static SPI_BUS: StaticCell<Mutex<CriticalSectionRawMutex, Spi<'static, SPI1, Async>>> =
    StaticCell::new();

const BLOCK_SIZE: usize = 512;

struct DbFlash<T> {
    flash: T,
    buffer: [u8; config::PAGE_SIZE],
}

impl<T: NorFlash + ReadNorFlash> flash::Flash for DbFlash<T> {
    type Error = T::Error;

    fn page_count(&self) -> usize {
        // config::MAX_PAGE_COUNT - 1
        4096
    }

    async fn erase(&mut self, page_id: PageID) -> Result<(), <DbFlash<T> as flash::Flash>::Error> {
        self.flash
            .erase(
                (page_id.index() * config::PAGE_SIZE) as u32,
                (page_id.index() * config::PAGE_SIZE + config::PAGE_SIZE) as u32,
            )
            .await
    }

    async fn read(
        &mut self,
        page_id: PageID,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), <DbFlash<T> as flash::Flash>::Error> {
        let address = page_id.index() * config::PAGE_SIZE + offset;
        self.flash
            .read(address as u32, &mut self.buffer[..data.len()])
            .await?;
        data.copy_from_slice(&self.buffer[..data.len()]);
        Ok(())
    }

    async fn write(
        &mut self,
        page_id: PageID,
        offset: usize,
        data: &[u8],
    ) -> Result<(), <DbFlash<T> as flash::Flash>::Error> {
        let address = page_id.index() * config::PAGE_SIZE + offset;
        self.buffer[..data.len()].copy_from_slice(data);
        self.flash
            .write(address as u32, &self.buffer[..data.len()])
            .await
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    defmt::info!("Hello World!");

    let sck = p.PIN_10;
    let mosi = p.PIN_11;
    let miso = p.PIN_12;
    let cs = Output::new(p.PIN_13, Level::High);

    let mut config = Config::default();
    config.frequency = 400_000;

    let mut spi = Spi::new(
        p.SPI1,
        sck,
        mosi,
        miso,
        p.DMA_CH0,
        p.DMA_CH1,
        config.clone(),
    );

    // Sd cards need to be clocked with a at least 74 cycles on their spi clock without the cs enabled,
    // sd_init is a helper function that does this for us.
    loop {
        match sd_init(&mut spi).await {
            Ok(_) => break,
            Err(e) => {
                defmt::warn!("Sd init error: {}", e);
                embassy_time::Timer::after_millis(10).await;
            }
        }
    }

    let spi_bus = SPI_BUS.init(Mutex::new(spi));

    let spid = SpiDeviceWithConfig::new(spi_bus, cs, config);
    let mut sd = SdSpi::<_, _, aligned::A4>::new(spid, embassy_time::Delay);

    loop {
        // Initialize the card
        if let Ok(_) = sd.init().await {
            // Increase the speed up to the SD max of 25mhz

            let mut config = Config::default();
            config.frequency = 25_000_000;
            sd.spi().set_config(config);
            defmt::info!("SD card initialization complete!");

            break;
        }
        defmt::info!("Failed to init card, retrying...");
        embassy_time::Delay.delay_ns(5000u32).await;
    }

    let inner = BufStream::<_, BLOCK_SIZE>::new(sd);

    let flash = DbFlash {
        flash: inner,
        buffer: [0u8; config::PAGE_SIZE],
    };

    let db = Database::<_, NoopRawMutex>::new(flash, ekv::Config::default());

    info!("Mounting ekv...");
    if db.mount().await.is_err() {
        info!("Formatting...");
        db.format().await.unwrap();
    }
    info!("Mounted ekv");

    info!("Storing items...");
    let mut wtx = db.write_transaction().await;
    for x in 0..500u32 {
        let bytes = x.to_be_bytes();
        wtx.write(&bytes, &bytes).await.unwrap();
    }
    wtx.commit().await.unwrap();
    info!("Items stored");

    let mut buf = [0u8; 128];
    info!("Retrieving items...");
    for x in 0..500u32 {
        let rtx = db.read_transaction().await;
        let item = rtx
            .read(&x.to_be_bytes(), &mut buf)
            .await
            .map(|n| &buf[..n])
            .unwrap();
        assert_eq!(item, &x.to_be_bytes());
    }
    info!("Items retrieved");

    info!("Done");

    loop {
        wfe();
    }
}
