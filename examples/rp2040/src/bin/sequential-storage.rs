#![no_std]
#![no_main]

use core::ops::Range;

use block_device_adapters::BufStream;
use cortex_m::asm::wfe;
use defmt::info;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDeviceWithConfig;
use embassy_executor::Spawner;
use embassy_rp::{
    gpio::{Level, Output},
    peripherals::*,
    spi::{Async, Config, Spi},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embedded_hal_async::delay::DelayNs;
use sdspi::{sd_init, SdSpi};
use sequential_storage::{
    cache::PageStateCache,
    erase_all,
    map::{fetch_item, store_item},
};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

static SPI_BUS: StaticCell<Mutex<CriticalSectionRawMutex, Spi<'static, SPI1, Async>>> =
    StaticCell::new();

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
    let mut sd = SdSpi::<_, _, aligned::A1>::new(spid, embassy_time::Delay);

    loop {
        // Initialize the card
        if let Ok(_) = sd.init().await {
            // Increase the speed up to the SD max of 25mhz

            let mut config = Config::default();
            config.frequency = 25_000_000;
            sd.spi().set_config(config);
            defmt::info!("Initialization complete!");

            break;
        }
        defmt::info!("Failed to init card, retrying...");
        embassy_time::Delay.delay_ns(5000u32).await;
    }

    let mut inner = BufStream::<_, 512>::new(sd);

    const ADDRESS_RANGE: Range<u32> = 0..10_240;
    const PAGE_COUNT: usize = (ADDRESS_RANGE.end - ADDRESS_RANGE.start) as usize / 512;
    let mut cache = PageStateCache::<PAGE_COUNT>::new();
    let mut data_buffer = [0u8; 128];

    info!("Erasing flash...");
    erase_all(&mut inner, ADDRESS_RANGE).await.unwrap();
    info!("Flash erased");

    info!("Storing 500 items...");
    for x in 0..500u32 {
        store_item(
            &mut inner,
            ADDRESS_RANGE,
            &mut cache,
            &mut data_buffer,
            &x,
            &x,
        )
        .await
        .unwrap();
    }
    info!("Items stored");

    info!("Retrieving items...");
    for x in 0..500 {
        let item: Option<u32> =
            fetch_item(&mut inner, ADDRESS_RANGE, &mut cache, &mut data_buffer, &x)
                .await
                .unwrap();
        assert_eq!(item, Some(x));
    }
    info!("Items retrieved");

    loop {
        wfe();
    }
}
