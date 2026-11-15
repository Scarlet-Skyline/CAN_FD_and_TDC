拓展任务二：5 Mbit/s CAN FD 与 TDC 实现说明

在要求高速传输数据的情况下，classical CAN 难以处理，换用 FD CAN 总线处理。启用BRS（Bit Rate Switch, 位速率切换）来调传输速度，TDC（Transmitter Delay Compensation，发送器延迟补偿）来负责在高速数据段（5 Mbit/s）中，保证发送节点能正确“监听到”自己发出的数据，防止误报错并导致总线崩溃。

#具体原理：CAN 协议要求发送节点必须一边发数据，一边通过收发器（TX 到 RX 引脚）回读总线上的电平，确认自己发出去的位没有被别人篡改。但在 5 Mbit/s 的高速下，位时间极短（200ns），而信号穿过收发器芯片是有物理延迟的（可能高达 100ns）。如果发送节点还在原来的时间点去采样，读到的可能还是上一个位的电平，就会误以为发生了“位错误（Bit Error）”。
TDC 的作用就是计算出这个物理延迟，并在数据段重新定义一个二次采样点（SSP）。发送节点不按原计划时间采样了，而是往后推迟一点，等信号从收发器绕一圈回来了，再在 SSP 点去采样，这样就绝不会读错了。

$\color{red}{\text{
注意：拓展1已做过 classical CAN 硬件测试的详细说明，故此处对此不再做详细说明。
}}$
重点在于 CAN FD 中特殊部分（如TDC/BRS等）的说明

一、 总体说明（入口与数据路径）
代码入口：程序入口位于 src/main.rs 的 main 函数。该函数初始化了时钟、外设，并启动了多个并发的异步任务。

数据路径：

接收（CAN → 主机）：can_rx_task 通过中断读取 FDCAN1 的 FD 帧，放入容量为 8 的 CAN_RX_CHANNEL 队列。control_port 任务从队列取出数据，通过 protocol.rs 格式化为十六进制字符串，经 USB CDC 发送给主机。

发送（主机 → CAN）：主机通过 USB 发送文本指令，control_port 解析后放入容量为 4 的 CAN_TX_CHANNEL 队列。can_tx_task 取出 FD 帧并调用 FDCAN 驱动发送到总线上。

并发与缓冲：基于 Embassy 异步框架。使用 Channel 实现任务间通信。当 CAN_RX_CHANNEL 满时，丢弃最新帧以保证接收任务不阻塞；当 CAN_TX_CHANNEL 满时，向主机返回 ERR BUSY。protocol.rs 中的解析缓冲区已扩大至 256 字节，足以容纳 64 字节（128 hex 字符）载荷转换后的长字符串。

关键配置：board.rs 中配置了 FDCAN1 的仲裁段和数据段时序，启用了 BRS 和 TDC，并设置为 AllowFdCanAndBRS （比如classical CAN用 classicalOnly即可） 模式。

二、位时序/采样点计算，TDC配置，FD帧处理和协议格式

1. 位时序参数计算

FDCAN 内核时钟（fdcansel 选择 PLL2_Q）为 80 MHz。
时间量子 Tq=1/80 MHz = 12.5ns 

仲裁段（1 Mbit/s）：

目标位时间 = 1/1 Mbit/s = 1000ns = 80Tq

参数：prescaler = 1，TSEG1 (seg1) = 59，TSEG2 (seg2) = 20，SJW = 20。

##
位时序（Bit Timing）定义了CAN总线上一个位（Bit）的时间长度是如何划分的，它决定了通信的波特率和采样点位置。一个位时间被划分为多个时间量子（Tq），通常包含以下几个段：

同步段（Sync_Seg）：用于同步总线上的各个节点，固定为1个Tq。

传播段（Prop_Seg）：用于补偿信号在总线上的物理传播延迟。

相位缓冲段1（Phase_Seg1） 和 相位缓冲段2（Phase_Seg2）：用于补偿节点间的时钟误差。

采样点（Sample Point）：位于Phase_Seg1结束处，是节点读取总线电平的时刻。

同步跳转宽度（SJW）：定义了在一次同步中，位时间可以被延长或缩短的最大Tq数。

在配置寄存器时，Prop_Seg和Phase_Seg1通常被合并称为TSEG1，Phase_Seg2被称为TSEG2。采样点位置的计算公式为：

Sample Point (%) = (Sync_Seg + TSEG1) / (Sync_Seg + TSEG1 + TSEG2) × 100%。
##

所以仲裁段采样点计算：
(1+59)/80=75%


数据段（5 Mbit/s）：

目标位时间 = 1/5 Mbit/s = 200 ns =16Tq


参数：prescaler = 1，TSEG1 (seg1) = 12，TSEG2 (seg2) = 3，SJW = 3。

采样点计算：
(1+12)/16=81.25%


2. TDC（发送延迟补偿）配置

单位与设置：在代码中，通过设置 transceiver_delay_compensation: true 启用了 TDC。

！！！关于 TDCO/TDCF 的说明：由于当前工程依赖的 embassy-stm32 = 0.6.0 版本 API 限制，DataBitTiming 结构体中未暴露 tdc_offset (TDCO，TDC偏移量，可用于配置SSP最终位置) 和 tdc_filter_window_length (TDCF，可以设定SSP最小位置，避免过早采样采到噪声) 字段。在当前配置下，硬件将使用默认参数或自动测量环路延迟来计算 SSP 位置。（board.rs中把这两行注释掉了，不然编译过不了）

注意：跑起来应该没问题，但是如果发现高速数据段通信不稳定，可能需要升级库版本以手动微调 TDCO/TDCF。

3. FD 帧处理与协议格式
帧格式：使用 FdFrame 类型替代原经典 CAN 的 Frame。通过 FrameTransmissionConfig::AllowFdCanAndBRS 允许发送 FD 帧并开启 BRS（位速率切换）。（board.rs中启用的）

主机协议边界：采用文本协议，以换行符 \n 作为帧边界。

发送示例：CAN TX 123 112233445566778899AABBCC（ID: 0x123，载荷为 12 字节）。

接收示例：RX 123 112233445566778899AABBCC。

在所使用的DMTool的上位机软件中，分别选择串口/CAN窗口可进行发送。仲裁段/数据段的波特率配置也可以在“设备配置中进行配置”。

长载荷处理：DLC（数据长度码）由硬件根据写入的字节数自动推断。protocol.rs 中的 Line 解析器将缓冲区增大到 256 字节，并将允许的十六进制字符长度上限从 16 提升至 128，确保 64 字节（128个 hex 字符）的载荷能够被完整解析并送入 FdFrame。

三、 上板验证预期
当前硬件行为均为预期设计，尚未上板验证。 编译成功证明代码具备生成固件的能力。后续上板时，需要保证DMTool满足以下条件：

匹配 1 Mbit/s 仲裁段和 5 Mbit/s 数据段的位时序。

开启 CAN FD 和 BRS 支持。

保证物理总线（CAN_H/CAN_L）终端电阻匹配，且收发器支持 5 Mbps 速率。

