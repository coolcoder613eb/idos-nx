use super::system::Timestamp;

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Date {
    pub day: u8,
    pub month: u8,
    pub year: u16,
}

impl core::fmt::Display for Date {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("{:02}-{:02}-{:04}", self.day, self.month, self.year))
    }
}

impl core::fmt::Debug for Date {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("Date({:02}-{:02}-{:04})", self.day, self.month, self.year))
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Time {
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
}

impl core::fmt::Display for Time {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("{:02}:{:02}:{:02}", self.hours, self.minutes, self.seconds))
    }
}

impl core::fmt::Debug for Time {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("Time({:02}:{:02}:{:02})", self.hours, self.minutes, self.seconds))
    }
}

impl Time {
    pub fn print_short_to_buffer(&self, buffer: &mut [u8]) {
        let hour_ten = self.hours / 10;
        let hour_one = self.hours % 10;

        let minute_ten = self.minutes / 10;
        let minute_one = self.minutes % 10;

        buffer[0] = hour_ten + 0x30;
        buffer[1] = hour_one + 0x30;
        buffer[2] = b':';
        buffer[3] = minute_ten + 0x30;
        buffer[4] = minute_one + 0x30;
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DateTime {
    pub date: Date,
    pub time: Time,
}

pub const SECONDS_IN_DAY: u32 = 60 * 60 * 24;

const MONTH_START_OFFSET: [u32; 12] = [
    0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334,
];

fn is_leap_year(year: u16) -> bool {
    let y = year as u32;
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub fn year_offset_from_days(days: u32) -> u32 {
    let hundredths = days * 100;
    hundredths / 36525
}

impl DateTime {
    pub fn from_unix_epoch(seconds: u32) -> DateTime {
        // from http://howardhinnant.github.io/date_algorithms.html
        // 719468 = shift from epoch start to 2000-03-01
        let days_shift = 719468;
        let days_in_era = 146097;
        let days = seconds / SECONDS_IN_DAY + days_shift;
        let era = days / days_in_era;
        let day_of_era = days % days_in_era;
        let year_of_era = (
            day_of_era -
            day_of_era / 1460 +
            day_of_era / 36524 -
            day_of_era / (days_in_era - 1)
        ) / 365;
        let mut year = year_of_era + era * 400;
        let day_of_year = day_of_era - (year_of_era * 365 + year_of_era / 4 - year_of_era / 100);
        let month_partial = (day_of_year * 5 + 2) / 153;
        let day_of_month = day_of_year - (153 * month_partial + 2) / 5 + 1;
        let month = if month_partial < 10 { month_partial + 3 } else { month_partial - 9 };

        if month <= 2 {
            year += 1;
        }

        let seconds_of_day = seconds - (seconds / SECONDS_IN_DAY) * SECONDS_IN_DAY;
        let hours = seconds_of_day / 60 / 60;
        let minutes_of_day = seconds_of_day / 60;
        let minutes = minutes_of_day - (hours * 60);
        let sec = seconds_of_day - (hours * 60 * 60) - (minutes * 60);

        DateTime {
            date: Date {
                day: day_of_month as u8,
                month: month as u8,
                year: year as u16,
            },

            time: Time {
                seconds: sec as u8,
                minutes: minutes as u8,
                hours: hours as u8,
            },
        }
    }

    pub fn to_timestamp(&self) -> Timestamp {
        if self.date.year < 1980 || self.date.month == 0 || self.date.month > 12 {
            return Timestamp(0);
        }

        let year = self.date.year - 1980;
        let quadrennials = year as u32 / 4;
        let year_remainder = year as u32 % 4;
        let mut days = quadrennials * (366 + 365 + 365 + 365) + year_remainder * 365;
        if year_remainder > 0 {
            days += 1;
        }
        days += MONTH_START_OFFSET[self.date.month as usize - 1];
        if self.date.month > 2 && is_leap_year(self.date.year) {
            days += 1; // leap day
        }
        days += self.date.day as u32 - 1;

        let timestamp = days * SECONDS_IN_DAY
            + self.time.hours as u32 * 60 * 60
            + self.time.minutes as u32 * 60
            + self.time.seconds as u32;

        Timestamp(timestamp)
    }

    pub fn from_timestamp(ts: Timestamp) -> Self {
        let days = ts.0 / SECONDS_IN_DAY;
        let raw_time = ts.0 % SECONDS_IN_DAY;
        let year_offset = year_offset_from_days(days);
        let quadrennial_days = days % (365 + 365 + 365 + 366);
        let year_days = if quadrennial_days > 365 {
            (quadrennial_days - 366) % 365
        } else {
            quadrennial_days
        };
        let mut month = 0;
        let mut leap = 0;
        while month < 12 && MONTH_START_OFFSET[month] + leap <= year_days {
            month += 1;
            if month == 2 && year_offset % 4 == 0 {
                // 2000 is a leap year, don't need to check 2100
                leap = 1;
            }
        }
        let mut day = year_days + 1 - MONTH_START_OFFSET[month - 1];
        if month > 2 {
            day -= leap;
        }

        let total_minutes = raw_time / 60;
        let seconds = raw_time % 60;
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;

        DateTime {
            date: Date {
                day: day as u8,
                month: month as u8,
                year: year_offset as u16 + 1980,
            },

            time: Time {
                seconds: seconds as u8,
                minutes: minutes as u8,
                hours: hours as u8,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{year_offset_from_days, Date, DateTime, Time, Timestamp};

    #[test_case]
    fn year_offset() {
        assert_eq!(year_offset_from_days(1), 0);
        assert_eq!(year_offset_from_days(365), 0);
        assert_eq!(year_offset_from_days(366), 1);
        assert_eq!(year_offset_from_days(366 + 365 + 365 + 365), 4);
        assert_eq!(year_offset_from_days(366 + 365 + 365 + 365 + 365), 4);
        assert_eq!(year_offset_from_days(366 + 365 + 365 + 365 + 366), 5);
    }

    #[test_case]
    fn extract_time() {
        let mut time = Timestamp(1).to_datetime().time;
        assert_eq!(time, Time { hours: 0, minutes: 0, seconds: 1 });
        time = Timestamp(16332).to_datetime().time;
        assert_eq!(time, Time { hours: 4, minutes: 32, seconds: 12 });
        time = Timestamp(93595).to_datetime().time;
        assert_eq!(time, Time { hours: 1, minutes: 59, seconds: 55 });
    }

    #[test_case]
    fn extract_date() {
        let mut date = Timestamp(10).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 1, year: 1980 });
        date = Timestamp(2592000).to_datetime().date;
        assert_eq!(date, Date { day: 31, month: 1, year: 1980 });
        date = Timestamp(2678400).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 2, year: 1980 });
        date = Timestamp(5097600).to_datetime().date;
        assert_eq!(date, Date { day: 29, month: 2, year: 1980 });
        date = Timestamp(5184000).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 3, year: 1980 });
        date = Timestamp(7862400).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 4, year: 1980 });
        date = Timestamp(31622400).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 1, year: 1981 });
        date = Timestamp(126230400).to_datetime().date;
        assert_eq!(date, Date { day: 1, month: 1, year: 1984 });
        date = Timestamp(131328000).to_datetime().date;
        assert_eq!(date, Date { day: 29, month: 2, year: 1984 });
        date = Timestamp(1278713001).to_datetime().date;
        assert_eq!(date, Date { day: 8, month: 7, year: 2020 });
    }

    #[test_case]
    fn to_timestamp() {
        let dt = Timestamp(1278713001).to_datetime();
        assert_eq!(dt.to_timestamp(), Timestamp(1278713001));
    }

    #[test_case]
    fn from_unix() {
        assert_eq!(
            DateTime::from_unix_epoch(951868800),
            DateTime {
                date: Date {
                    day: 1,
                    month: 3,
                    year: 2000,
                },
                time: Time {
                    seconds: 0,
                    minutes: 0,
                    hours: 0,
                },
            },
        );

        assert_eq!(
            DateTime::from_unix_epoch(1635291103),
            DateTime {
                date: Date {
                    day: 26,
                    month: 10,
                    year: 2021,
                },
                time: Time {
                    hours: 23,
                    minutes: 31,
                    seconds: 43,
                },
            },
        );
    }
}

