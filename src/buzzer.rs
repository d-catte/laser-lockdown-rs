use alloc::vec;
use embassy_time::Duration;
use esp_hal::ledc::channel::{Channel, ChannelIFace};
use esp_hal::ledc::{timer, LowSpeed};
use esp_hal::ledc::timer::{Timer, TimerIFace};
use esp_hal::ledc::timer::config::Duty;
use esp_hal::time::Rate;

#[derive(Copy, Clone)]
enum Note {
    C, Cs, D, Ds, E, F, Fs, G, Gs, A, As, B, Pause,
}

impl Note {
    // Base frequencies for Octave 4 (Concert Pitch)
    const FREQS_O4: [u32; 12] = [
        262, 277, 294, 311, 330, 349, 370, 392, 415, 440, 466, 494
    ];

    fn to_freq(self, octave: i32) -> u32 {
        if let Note::Pause = self { return 0; }

        // Handle Bb Transposition:
        let written_index = self as usize;
        let concert_index = (written_index + 10) % 12;

        // Adjust octave if the transposition wraps around
        let adj_octave = if written_index < 2 {
            octave - 1
        } else {
            octave
        };

        let base_freq = Self::FREQS_O4[concert_index];

        // Shift frequency based on octave relative to Octave 4
        let diff = adj_octave - 4;
        if diff >= 0 {
            base_freq << diff
        } else {
            base_freq >> diff.abs()
        }
    }
}

struct Beat;
impl Beat {
    const Q: u32 = 428;              // Quarter: 60,000 / 140
    const H: u32 = Self::Q * 2;      // Half
    const DH: u32 = Self::Q * 3;     // Dotted Half
    const E: u32 = Self::Q / 2;      // Eighth
    const DE: u32 = Self::E + (Self::E / 2); // Dotted Eighth
    const S: u32 = Self::Q / 4;      // Sixteenth
}

pub async fn play_fight_song(channel: Channel<'_, LowSpeed>, timer: &mut Timer<'_, LowSpeed>) {
    // Note, Octave, Length
    let fight_song = vec![
        (Note::D, 5, Beat::DE),
        (Note::Cs, 5, Beat::S),
        (Note::C, 5, Beat::E),
        (Note::B, 5, Beat::E),
        (Note::A, 5, Beat::E),
        (Note::G, 4, Beat::E),
        (Note::Fs, 4, Beat::E),
        (Note::E, 4, Beat::E),
        (Note::D, 4, Beat::E),
        (Note::Pause, 4, Beat::E),
        (Note::Pause, 4, Beat::Q),
        (Note::D, 4, Beat::E),
        (Note::Pause, 4, Beat::E),
        (Note::Pause, 4, Beat::Q),
        (Note::B, 5, Beat::H),
        (Note::B, 5, Beat::Q),
        (Note::B, 5, Beat::Q),
    ];

    for &(note_name, octave, duration) in fight_song.iter() {
        let freq = note_name.to_freq(octave);

        if freq == 0 {
            channel.set_duty(0).unwrap();
        } else {
            // Change the hardware frequency
            timer.configure(timer::config::Config {
                duty: Duty::Duty1Bit,
                frequency: Rate::from_hz(freq),
                clock_source: timer::LSClockSource::APBClk,
            }).unwrap();

            channel.set_duty(50).unwrap();
        }

        embassy_time::Timer::after(Duration::from_millis(duration as u64)).await;

        // 10ms between notes for tonguing effect
        channel.set_duty(0).unwrap();
        embassy_time::Timer::after(Duration::from_millis(10)).await;
    }
}