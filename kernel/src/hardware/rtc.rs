use crate::arch::port::Port;
use crate::time::date::{Date, DateTime, Time};

pub struct RtcTime {
    seconds: u8,
    minutes: u8,
    hours: u8,

    day: u8,
    month: u8,
    year: u8,
}

impl RtcTime {
    pub fn to_datetime(&self) -> DateTime {
        DateTime {
            date: Date {
                day: self.day,
                month: self.month,
                year: self.year as u16 + 2000,
            },

            time: Time {
                seconds: self.seconds,
                minutes: self.minutes,
                hours: self.hours,
            },
        }
    }
}

fn convert_bcd(bcd: u8) -> u8 {
    let tens = bcd >> 4;
    let ones = bcd & 0x0f;
    tens * 10 + ones
}

fn read_rtc_register(index: u8) -> u8 {
    Port::new(0x70).write_u8(index);
    Port::new(0x71).read_u8()
}

pub fn read_rtc_time() -> RtcTime {
    let nmi_flag = Port::new(0x70).read_u8() & 0x80;
    let reg_b = read_rtc_register(nmi_flag | 0x0b);
    let use_24_hour = reg_b & 2 != 0;
    let use_bcd = reg_b & 4 == 0;

    let mut time = RtcTime {
        seconds: 0,
        minutes: 0,
        hours: 0,

        day: 0,
        month: 0,
        year: 0,
    };

    time.seconds    = read_rtc_register(nmi_flag | 0x00);
    time.minutes    = read_rtc_register(nmi_flag | 0x02);
    time.hours      = read_rtc_register(nmi_flag | 0x04);
    time.day        = read_rtc_register(nmi_flag | 0x07);
    time.month      = read_rtc_register(nmi_flag | 0x08);
    time.year       = read_rtc_register(nmi_flag | 0x09);

    if use_bcd {
        time.seconds = convert_bcd(time.seconds);
        time.minutes = convert_bcd(time.minutes);
        time.day = convert_bcd(time.day);
        time.month = convert_bcd(time.month);
        time.year = convert_bcd(time.year);

        if !use_24_hour {
            let pm = time.hours & 0x80 != 0;
            time.hours = convert_bcd(time.hours & 0x7f);
            time.hours %= 12;
            if pm {
                time.hours += 12;
            }
        } else {
            time.hours = convert_bcd(time.hours);
        }
    } else {
        if !use_24_hour {
            let pm = time.hours & 0x80 != 0;
            time.hours = time.hours & 0x7f;
            time.hours %= 12;
            if pm {
                time.hours += 12;
            }
        }
    }

    time
}
