#![no_std]
#![no_main]

mod board;
mod dap_usb;
mod protocol;
pub mod swd;
mod wiring;

use board::Usb;
use embassy_executor::Spawner;
use embassy_futures::{
    join::{join, join4},
    select::{Either, select},
};
//use embassy_stm32::usart::BufferedUart;
use embassy_stm32::{
    can::{CanRx, CanTx, frame::FdFrame},
    usart::BufferedUart,
};
//use embassy_time::{Duration, Ticker};
use embassy_usb::{
    Builder,
    class::cdc_acm::{CdcAcmClass, State},
};
use embedded_io_async::{Read, Write};
use panic_halt as _;
use static_cell::StaticCell;
use swd::Swd;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
const PACKET: usize = 64;
// Track linker-script changes so Cargo relinks the firmware.
const _: &[u8] = include_bytes!("../memory.x");
// use embassy_sync::{
//     blocking_mutex::raw::CriticalSectionRawMutex,
//     channel::Channel,
// };
/// CAN 接收 → 主机发送：容量 8 帧，满时丢弃最旧帧
static CAN_RX_CHANNEL: Channel<CriticalSectionRawMutex, FdFrame, 8> = Channel::new();

/// 主机请求 → CAN 发送：容量 4 帧，满时回 ERR BUSY
static CAN_TX_CHANNEL: Channel<CriticalSectionRawMutex, FdFrame, 4> = Channel::new();

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let (driver, ports, swd) = wiring::configure(board::init());
    // Classroom-only test VID/PID, not allocated product identifiers.
    let mut config = embassy_usb::Config::new(0xC0DE, 0xCAFC);
    config.manufacturer = Some("HerKules");
    config.product = Some("DM-MC02 CMSIS-DAP Demo");
    config.max_packet_size_0 = PACKET as u8;
    static DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 64]> = StaticCell::new();
    static CTL_STATE: StaticCell<State> = StaticCell::new();
    static UART_STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        DESC.init([0; 256]),
        BOS.init([0; 128]),
        MSOS.init([0; 256]),
        CONTROL.init([0; 64]),
    );
    builder.msos_descriptor(0x0603_0000, 0x20);
    let dap = dap_usb::DapUsb::new(&mut builder);
    let control = CdcAcmClass::new(&mut builder, CTL_STATE.init(State::new()), PACKET as u16);
    let serial = CdcAcmClass::new(&mut builder, UART_STATE.init(State::new()), PACKET as u16);
    let mut device = builder.build();
    let (can_tx, can_rx, _props) = ports.can.split();
    // spawner.spawn(can_rx_task(can_rx)).unwrap();
    // spawner.spawn(can_tx_task(can_tx)).unwrap();
    // join4(
    // device.run(),
    // can_rx_task(can_rx),
    // can_tx_task(can_tx),
    // control_port(control, swd),
    // // 如果要保留 UART 透传，可以用 join5，把 uart_port 也放进来
    // );
    join(
        join4(
            device.run(),
            control_port(control, swd),
            uart_port(serial, ports.uart),
            dap.run(),
        ),
        join(can_rx_task(can_rx), can_tx_task(can_tx)),
    )
    .await;
}

async fn control_port(
    mut usb: CdcAcmClass<'static, Usb>,
    //mut can: Can<'static>,
    mut swd: Option<Swd>,
) {
    let mut packet = [0; PACKET];
    //let mut ticker = Ticker::every(Duration::from_millis(500)); // 每 500ms 触发一次
    loop {
        usb.wait_connection().await;
        let mut line = protocol::Line::new();

        loop {
            let read_usb = async { usb.read_packet(&mut packet).await.ok() };
            let read_can = async { CAN_RX_CHANNEL.receive().await };

            match select(read_usb, read_can).await {
                // 1. 收到来自主机的 USB 数据
                Either::First(Some(n)) => {
                    for &byte in &packet[..n] {
                        if let Some(command) = line.push(byte) {
                            match protocol::execute(command, &mut swd).await {
                                protocol::Action::Reply(reply) => {
                                    if usb.write_packet(reply.as_bytes()).await.is_err() {
                                        break; // USB断连
                                    }
                                }
                                protocol::Action::SendCan(frame) => {
                                    match CAN_TX_CHANNEL.try_send(frame) {
                                        Ok(()) => {
                                            let _ = usb.write_packet(b"OK QUEUED\n").await;
                                        }
                                        Err(_) => {
                                            let _ = usb.write_packet(b"ERR BUSY\n").await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // 2. USB 读取错误或断开
                Either::First(None) => break,
                // 3. 收到来自 CAN 的帧，主动推送给主机
                Either::Second(frame) => {
                    let text = protocol::format_rx(&frame);
                    if usb.write_packet(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

//#[embassy_executor::task]
async fn can_rx_task(mut rx: CanRx<'static>) {
    loop {
        let envelope = match rx.read_fd().await {
            Ok(e) => e,
            Err(_) => continue, // 总线错误，继续接收
        };
        let frame = envelope.frame;

        // 有界缓冲：满时丢弃最旧帧，保证 CAN 接收不被阻塞
        let _ = CAN_RX_CHANNEL.try_send(frame);
    }
}

//#[embassy_executor::task]
async fn can_tx_task(mut tx: CanTx<'static>) {
    loop {
        let frame = CAN_TX_CHANNEL.receive().await;
        // 用 try_write 非阻塞发送；队列满时短暂等待后重试
        let _ = tx.write_fd(&frame).await;
        //embassy_time::Timer::after(embassy_time::Duration::from_millis(1)).await;
        // match CAN_TX_CHANNEL.try_send(frame) {
        //     Ok(_) => break,
        //     Err(_) => {
        //         embassy_time::Timer::after(embassy_time::Duration::from_millis(1)).await;
        //     }
        // }
    }
}

// async fn usb_tx_task(mut usb_tx: <Usb as ...>::EndpointIn) {
//     loop {
//         let frame = CAN_RX_CHANNEL.receive().await;
//         let text = protocol::format_rx(&frame); // "RX <ID> <DATA>\n"
//         let _ = usb_tx.write_packet(text.as_bytes()).await;
//     }
// }

async fn uart_port(usb: CdcAcmClass<'static, Usb>, uart: Option<BufferedUart<'static>>) {
    let Some(uart) = uart else {
        return;
    };
    let (mut serial_out, mut serial_in) = uart.split();
    let (mut usb_out, mut usb_in) = usb.split();
    loop {
        usb_in.wait_connection().await;
        let from_host = async {
            let mut buf = [0; PACKET];
            while let Ok(n) = usb_in.read_packet(&mut buf).await {
                if serial_out.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        let to_host = async {
            // A short packet needs no zero-length terminator.
            let mut buf = [0; PACKET - 1];
            while let Ok(n) = serial_in.read(&mut buf).await {
                if n != 0 && usb_out.write_packet(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        select(from_host, to_host).await;
    }
}
