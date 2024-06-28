//! HAL interface to the SPIM peripheral.
//!
//! See product specification, chapter 31.

use core::ops::Deref;
use core::sync::atomic::{compiler_fence, Ordering::SeqCst};
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{self, ErrorKind, ErrorType, SpiBus};

#[cfg(any(feature = "9160", feature = "9120", feature = "5340-app", feature = "5340-net"))]
use crate::pac::{spim0_ns as spim0, SPIM0_NS as SPIM0};

#[cfg(any(feature = "9160", feature = "9120"))]
use crate::pac::{SPIM1_NS as SPIM1, SPIM2_NS as SPIM2, SPIM3_NS as SPIM3};

#[cfg(not(any(feature = "9160", feature = "9120", feature = "5340-app", feature = "5340-net")))]
use crate::pac::{spim0, SPIM0};

pub use embedded_hal::spi::{Mode, Phase, Polarity, MODE_0, MODE_1, MODE_2, MODE_3};
pub use spim0::frequency::FREQUENCY_A as Frequency;

use core::iter::repeat_with;

#[cfg(feature = "52811")]
use crate::pac::SPIM1;

#[cfg(any(feature = "52832", feature = "52833", feature = "52840"))]
use crate::pac::{SPIM1, SPIM2};

#[cfg(any(feature = "52833", feature = "52840"))]
use crate::pac::SPIM3;

use crate::gpio::{Floating, Input, Output, Pin, PushPull};
use crate::target_constants::{EASY_DMA_SIZE, FORCE_COPY_BUFFER_SIZE};
use crate::{slice_in_ram, slice_in_ram_or, DmaSlice};

/// Interface to a SPIM instance.
///
/// This is a very basic interface that comes with the following limitations:
/// - The SPIM instances share the same address space with instances of SPIS,
///   SPI, TWIM, TWIS, and TWI. You need to make sure that conflicting instances
///   are disabled before using `Spim`. See product specification, section 15.2.
pub struct Spim<T>(T);

impl<T> ErrorType for Spim<T> {
    type Error = Error;
}

impl<T: Instance> SpiBus for Spim<T> {
    fn read(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        // A mutable slice can only be built from data in RAM.
        assert!(slice_in_ram(words));

        for chunk in words.chunks(EASY_DMA_SIZE) {
            self.do_spi_dma_transfer(DmaSlice::null(), DmaSlice::from_slice(chunk))?;
        }
        Ok(())
    }

    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        if slice_in_ram(words) {
            for chunk in words.chunks(EASY_DMA_SIZE) {
                self.do_spi_dma_transfer(DmaSlice::from_slice(chunk), DmaSlice::null())?;
            }
        } else {
            let mut buf = [0u8; FORCE_COPY_BUFFER_SIZE];
            for chunk in words.chunks(FORCE_COPY_BUFFER_SIZE) {
                buf[..chunk.len()].copy_from_slice(chunk);
                self.do_spi_dma_transfer(
                    DmaSlice::from_slice(&buf[..chunk.len()]),
                    DmaSlice::null(),
                )?;
            }
        }

        Ok(())
    }

    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        self.transfer_split_uneven_internal(write, read)
    }

    fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        // A mutable slice can only be built from data in RAM.
        assert!(slice_in_ram(words));

        words.chunks(EASY_DMA_SIZE).try_for_each(|chunk| {
            self.do_spi_dma_transfer(DmaSlice::from_slice(chunk), DmaSlice::from_slice(chunk))
        })?;

        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // This implementation doesn't buffer operations, so there is nothing to flush.
        Ok(())
    }
}

#[cfg(feature = "embedded-hal-02")]
impl<T> embedded_hal_02::blocking::spi::Transfer<u8> for Spim<T>
where
    T: Instance,
{
    type Error = Error;

    fn transfer<'w>(&mut self, words: &'w mut [u8]) -> Result<&'w [u8], Error> {
        // If the slice isn't in RAM, we can't write back to it at all
        slice_in_ram_or(words, Error::DMABufferNotInDataMemory)?;

        words.chunks(EASY_DMA_SIZE).try_for_each(|chunk| {
            self.do_spi_dma_transfer(DmaSlice::from_slice(chunk), DmaSlice::from_slice(chunk))
        })?;

        Ok(words)
    }
}

#[cfg(feature = "embedded-hal-02")]
impl<T> embedded_hal_02::blocking::spi::Write<u8> for Spim<T>
where
    T: Instance,
{
    type Error = Error;

    fn write<'w>(&mut self, words: &'w [u8]) -> Result<(), Error> {
        // Mask on segment where Data RAM is located on nrf52840 and nrf52832
        // Upper limit is choosen to entire area where DataRam can be placed
        let needs_copy = !slice_in_ram(words);

        let chunk_sz = if needs_copy {
            FORCE_COPY_BUFFER_SIZE
        } else {
            EASY_DMA_SIZE
        };

        let step = if needs_copy {
            Self::spi_dma_copy
        } else {
            Self::spi_dma_no_copy
        };

        words.chunks(chunk_sz).try_for_each(|c| step(self, c))
    }
}

impl<T> Spim<T>
where
    T: Instance,
{
    #[cfg(feature = "embedded-hal-02")]
    fn spi_dma_no_copy(&mut self, chunk: &[u8]) -> Result<(), Error> {
        self.do_spi_dma_transfer(DmaSlice::from_slice(chunk), DmaSlice::null())
    }

    #[cfg(feature = "embedded-hal-02")]
    fn spi_dma_copy(&mut self, chunk: &[u8]) -> Result<(), Error> {
        let mut buf = [0u8; FORCE_COPY_BUFFER_SIZE];
        buf[..chunk.len()].copy_from_slice(chunk);

        self.do_spi_dma_transfer(DmaSlice::from_slice(&buf[..chunk.len()]), DmaSlice::null())
    }

    pub fn new(spim: T, pins: Pins, frequency: Frequency, mode: Mode, orc: u8) -> Self {
        // Select pins.
        match pins.sck {
            Some(sck) => spim.psel.sck.write(|w| {
                unsafe { w.bits(sck.psel_bits()) };
                w.connect().connected()
            }),
            None => spim.psel.sck.write(|w| w.connect().disconnected()),
        }
        match pins.mosi {
            Some(mosi) => spim.psel.mosi.write(|w| {
                unsafe { w.bits(mosi.psel_bits()) };
                w.connect().connected()
            }),
            None => spim.psel.mosi.write(|w| w.connect().disconnected()),
        }
        match pins.miso {
            Some(miso) => spim.psel.miso.write(|w| {
                unsafe { w.bits(miso.psel_bits()) };
                w.connect().connected()
            }),
            None => spim.psel.miso.write(|w| w.connect().disconnected()),
        }

        // Enable SPIM instance.
        spim.enable.write(|w| w.enable().enabled());

        // Configure mode.
        spim.config.write(|w| {
            // Can't match on `mode` due to embedded-hal, see https://github.com/rust-embedded/embedded-hal/pull/126
            if mode == MODE_0 {
                w.order().msb_first();
                w.cpol().active_high();
                w.cpha().leading();
            } else if mode == MODE_1 {
                w.order().msb_first();
                w.cpol().active_high();
                w.cpha().trailing();
            } else if mode == MODE_2 {
                w.order().msb_first();
                w.cpol().active_low();
                w.cpha().leading();
            } else {
                w.order().msb_first();
                w.cpol().active_low();
                w.cpha().trailing();
            }
            w
        });

        // Configure frequency.
        spim.frequency.write(|w| w.frequency().variant(frequency));

        // Set over-read character to `0`.
        spim.orc.write(|w|
            // The ORC field is 8 bits long, so `0` is a valid value to write
            // there.
            unsafe { w.orc().bits(orc) });

        Spim(spim)
    }

    /// Internal helper function to setup and execute SPIM DMA transfer.
    fn do_spi_dma_transfer(&mut self, tx: DmaSlice, rx: DmaSlice) -> Result<(), Error> {
        // Conservative compiler fence to prevent optimizations that do not
        // take in to account actions by DMA. The fence has been placed here,
        // before any DMA action has started.
        compiler_fence(SeqCst);

        // Set up the DMA write.
        self.0.txd.ptr.write(|w| unsafe { w.ptr().bits(tx.ptr) });

        self.0.txd.maxcnt.write(|w|
            // Note that that nrf52840 maxcnt is a wider.
            // type than a u8, so we use a `_` cast rather than a `u8` cast.
            // The MAXCNT field is thus at least 8 bits wide and accepts the full
            // range of values that fit in a `u8`.
            unsafe { w.maxcnt().bits(tx.len as _ ) });

        // Set up the DMA read.
        self.0.rxd.ptr.write(|w|
            // This is safe for the same reasons that writing to TXD.PTR is
            // safe. Please refer to the explanation there.
            unsafe { w.ptr().bits(rx.ptr) });
        self.0.rxd.maxcnt.write(|w|
            // This is safe for the same reasons that writing to TXD.MAXCNT is
            // safe. Please refer to the explanation there.
            unsafe { w.maxcnt().bits(rx.len as _) });

        // Start SPI transaction.
        self.0.tasks_start.write(|w|
            // `1` is a valid value to write to task registers.
            unsafe { w.bits(1) });

        // Conservative compiler fence to prevent optimizations that do not
        // take in to account actions by DMA. The fence has been placed here,
        // after all possible DMA actions have completed.
        compiler_fence(SeqCst);

        // Wait for END event.
        //
        // This event is triggered once both transmitting and receiving are
        // done.
        while self.0.events_end.read().bits() == 0 {}

        // Reset the event, otherwise it will always read `1` from now on.
        self.0.events_end.write(|w| w);

        // Conservative compiler fence to prevent optimizations that do not
        // take in to account actions by DMA. The fence has been placed here,
        // after all possible DMA actions have completed.
        compiler_fence(SeqCst);

        if self.0.txd.amount.read().bits() != tx.len {
            return Err(Error::Transmit);
        }
        if self.0.rxd.amount.read().bits() != rx.len {
            return Err(Error::Receive);
        }
        Ok(())
    }

    /// Read and write from a SPI slave, using a single buffer.
    ///
    /// This method implements a complete read transaction, which consists of
    /// the master transmitting what it wishes to read, and the slave responding
    /// with the requested data.
    ///
    /// Uses the provided chip select pin to initiate the transaction. Transmits
    /// all bytes in `buffer`, then receives an equal number of bytes.
    pub fn transfer(
        &mut self,
        chip_select: &mut Pin<Output<PushPull>>,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        slice_in_ram_or(buffer, Error::DMABufferNotInDataMemory)?;

        chip_select.set_low().unwrap();

        // Don't return early, as we must reset the CS pin.
        let res = buffer.chunks(EASY_DMA_SIZE).try_for_each(|chunk| {
            self.do_spi_dma_transfer(DmaSlice::from_slice(chunk), DmaSlice::from_slice(chunk))
        });

        chip_select.set_high().unwrap();

        res
    }

    /// Read and write from a SPI slave, using separate read and write buffers.
    ///
    /// This method implements a complete read transaction, which consists of
    /// the master transmitting what it wishes to read, and the slave responding
    /// with the requested data.
    ///
    /// Uses the provided chip select pin to initiate the transaction. Transmits
    /// all bytes in `tx_buffer`, then receives bytes until `rx_buffer` is full.
    ///
    /// If `tx_buffer.len() != rx_buffer.len()`, the transaction will stop at the
    /// smaller of either buffer.
    pub fn transfer_split_even(
        &mut self,
        chip_select: &mut Pin<Output<PushPull>>,
        tx_buffer: &[u8],
        rx_buffer: &mut [u8],
    ) -> Result<(), Error> {
        // NOTE: RAM slice check for `rx_buffer` is not necessary, as a mutable
        // slice can only be built from data located in RAM.
        slice_in_ram_or(tx_buffer, Error::DMABufferNotInDataMemory)?;

        let txi = tx_buffer.chunks(EASY_DMA_SIZE);
        let rxi = rx_buffer.chunks_mut(EASY_DMA_SIZE);

        chip_select.set_low().unwrap();

        // Don't return early, as we must reset the CS pin
        let res = txi.zip(rxi).try_for_each(|(t, r)| {
            self.do_spi_dma_transfer(DmaSlice::from_slice(t), DmaSlice::from_slice(r))
        });

        chip_select.set_high().unwrap();

        res
    }

    /// Read and write from a SPI slave, using separate read and write buffers.
    ///
    /// This method implements a complete read transaction, which consists of
    /// the master transmitting what it wishes to read, and the slave responding
    /// with the requested data.
    ///
    /// Uses the provided chip select pin to initiate the transaction. Transmits
    /// all bytes in `tx_buffer`, then receives bytes until `rx_buffer` is full.
    ///
    /// This method is more complicated than the other `transfer` methods because
    /// it is allowed to perform transactions where `tx_buffer.len() != rx_buffer.len()`.
    /// If this occurs, extra incoming bytes will be discarded, OR extra outgoing bytes
    /// will be filled with the `orc` value.
    pub fn transfer_split_uneven(
        &mut self,
        chip_select: &mut Pin<Output<PushPull>>,
        tx_buffer: &[u8],
        rx_buffer: &mut [u8],
    ) -> Result<(), Error> {
        slice_in_ram_or(tx_buffer, Error::DMABufferNotInDataMemory)?;

        chip_select.set_low().unwrap();
        // Don't return early, as we must reset the CS pin.
        let res = self.transfer_split_uneven_internal(tx_buffer, rx_buffer);
        chip_select.set_high().unwrap();
        res
    }

    pub fn transfer_split_uneven_internal(
        &mut self,
        tx_buffer: &[u8],
        rx_buffer: &mut [u8],
    ) -> Result<(), Error> {
        // NOTE: RAM slice check for `rx_buffer` is not necessary, as a mutable
        // slice can only be built from data located in RAM.
        if slice_in_ram(tx_buffer) {
            // For the tx and rx, we want to return a DmaSlice with a chunk as long
            // as there is data to send. We then chain a repeat to the end so once
            // all chunks have been exhausted, we will keep DmaSlice::null() out of
            // the iterators.
            let txi = tx_buffer
                .chunks(EASY_DMA_SIZE)
                .map(|chunk| DmaSlice::from_slice(chunk))
                .chain(repeat_with(DmaSlice::null));
            let rxi = rx_buffer
                .chunks_mut(EASY_DMA_SIZE)
                .map(|chunk| DmaSlice::from_slice(chunk))
                .chain(repeat_with(DmaSlice::null));

            // We then chain the iterators together, and once BOTH are giving null
            // DmaSlices, then we are done sending and receiving.
            for (t, r) in txi.zip(rxi).take_while(|(t, r)| t.ptr != 0 || r.ptr != 0) {
                self.do_spi_dma_transfer(t, r)?;
            }
        } else {
            let mut buf = [0u8; FORCE_COPY_BUFFER_SIZE];
            let txi = tx_buffer
                .chunks(FORCE_COPY_BUFFER_SIZE)
                .map(Some)
                .chain(repeat_with(|| None));
            let rxi = rx_buffer
                .chunks_mut(FORCE_COPY_BUFFER_SIZE)
                .map(|chunk| DmaSlice::from_slice(chunk))
                .chain(repeat_with(DmaSlice::null));
            for (tx_chunk, r) in txi.zip(rxi).take_while(|(t, r)| t.is_some() || r.ptr != 0) {
                let t = if let Some(tx_chunk) = tx_chunk {
                    buf[..tx_chunk.len()].copy_from_slice(tx_chunk);
                    DmaSlice::from_slice(&buf[..tx_chunk.len()])
                } else {
                    DmaSlice::null()
                };
                self.do_spi_dma_transfer(t, r)?;
            }
        }

        Ok(())
    }

    /// Write to an SPI slave.
    ///
    /// This method uses the provided chip select pin to initiate the
    /// transaction, then transmits all bytes in `tx_buffer`. All incoming
    /// bytes are discarded.
    pub fn write(
        &mut self,
        chip_select: &mut Pin<Output<PushPull>>,
        tx_buffer: &[u8],
    ) -> Result<(), Error> {
        slice_in_ram_or(tx_buffer, Error::DMABufferNotInDataMemory)?;
        self.transfer_split_uneven(chip_select, tx_buffer, &mut [0u8; 0])
    }

    /// Return the raw interface to the underlying SPIM peripheral.
    pub fn free(self) -> (T, Pins) {
        let sck = self.0.psel.sck.read();
        let mosi = self.0.psel.mosi.read();
        let miso = self.0.psel.miso.read();
        self.0.psel.sck.reset();
        self.0.psel.mosi.reset();
        self.0.psel.miso.reset();
        (
            self.0,
            Pins {
                sck: if sck.connect().is_connected() {
                    Some(unsafe { Pin::from_psel_bits(sck.bits()) })
                } else {
                    None
                },
                mosi: if mosi.connect().is_connected() {
                    Some(unsafe { Pin::from_psel_bits(mosi.bits()) })
                } else {
                    None
                },
                miso: if miso.connect().is_connected() {
                    Some(unsafe { Pin::from_psel_bits(miso.bits()) })
                } else {
                    None
                },
            },
        )
    }
}

/// GPIO pins for SPIM interface
pub struct Pins {
    /// SPI clock.
    ///
    /// None if unused.
    pub sck: Option<Pin<Output<PushPull>>>,

    /// MOSI Master out, slave in
    /// None if unused
    pub mosi: Option<Pin<Output<PushPull>>>,

    /// MISO Master in, slave out
    /// None if unused
    pub miso: Option<Pin<Input<Floating>>>,
}

#[derive(Debug)]
pub enum Error {
    /// EasyDMA can only read from data memory, read only buffers in flash will fail.
    DMABufferNotInDataMemory,
    Transmit,
    Receive,
}

impl spi::Error for Error {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// Implemented by all SPIM instances.
pub trait Instance: Deref<Target = spim0::RegisterBlock> + sealed::Sealed {}

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for SPIM0 {}
impl Instance for SPIM0 {}

#[cfg(any(
    feature = "52832",
    feature = "52833",
    feature = "52840",
    feature = "52811",
    feature = "9160",
    feature = "9120"
))]
mod _spim1 {
    use super::*;
    impl Instance for SPIM1 {}
    impl sealed::Sealed for SPIM1 {}
}

#[cfg(any(
    feature = "52832",
    feature = "52833",
    feature = "52840",
    feature = "9160",
    feature = "9120"
))]
mod _spim2 {
    use super::*;
    impl Instance for SPIM2 {}
    impl sealed::Sealed for SPIM2 {}
}

#[cfg(any(feature = "52833", feature = "52840", feature = "9160", feature = "9120"))]
mod _spim3 {
    use super::*;
    impl Instance for SPIM3 {}
    impl sealed::Sealed for SPIM3 {}
}
