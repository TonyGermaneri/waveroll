#pragma once
#include <waveroll.h>

#include <juce_audio_formats/juce_audio_formats.h>
#include <juce_audio_processors/juce_audio_processors.h>

/**
 * A tap. It reports zero latency, adds no tail, and does not touch the buffer.
 *
 * Bit transparency is structural rather than careful: processBlock reads and returns, so there is
 * no line of code in the audio path that *could* alter the signal. That matters because people
 * will put this on a mix bus.
 */
class WaverollProcessor : public juce::AudioProcessor
{
public:
    WaverollProcessor();
    ~WaverollProcessor() override;

    void prepareToPlay (double sampleRate, int samplesPerBlock) override;
    void releaseResources() override {}
    bool isBusesLayoutSupported (const BusesLayout&) const override;
    void processBlock (juce::AudioBuffer<float>&, juce::MidiBuffer&) override;

    juce::AudioProcessorEditor* createEditor() override;
    bool hasEditor() const override { return true; }

    const juce::String getName() const override { return "Waveroll"; }
    bool acceptsMidi() const override { return true; }
    bool producesMidi() const override { return false; }
    bool isMidiEffect() const override { return false; }
    // No tail: nothing here rings, and claiming one makes a host hold the plugin open after the
    // material has finished.
    double getTailLengthSeconds() const override { return 0.0; }

    int getNumPrograms() override { return 1; }
    int getCurrentProgram() override { return 0; }
    void setCurrentProgram (int) override {}
    const juce::String getProgramName (int) override { return {}; }
    void changeProgramName (int, const juce::String&) override {}

    // Settings only, never the ring: a 67 MB chunk in somebody's project file is a support
    // incident, and the buffer is memory-only by design anyway.
    void getStateInformation (juce::MemoryBlock&) override;
    void setStateInformation (const void*, int) override;

    /**
     * Things the editor should do, requested from anywhere.
     *
     * A bitmask of atomics rather than a queue, because every action here is idempotent within a
     * frame -- asking twice to send is asking once -- and because the audio thread will be one of
     * the callers once MIDI bindings exist. Setting a bit is wait-free; doing the work is not, and
     * belongs on the message thread where files may be written.
     */
    enum class Action { Send = 1, Mark = 2, Hold = 4, FromMarker = 8, Downbeat = 16 };
    void request (Action action) noexcept
    {
        pending.fetch_or ((int) action, std::memory_order_release);
    }
    /** Takes everything requested since the last call. */
    int takeRequests() noexcept { return pending.exchange (0, std::memory_order_acquire); }

    /** The Rust core. Owned here because the audio thread writes into it and the editor comes
        and goes; an editor-owned core would lose the buffer every time the window closed. */
    void* core() const noexcept { return rustCore; }
    double currentSampleRate() const noexcept { return sampleRate.load(); }

private:
    /** Frames of ring, as a power of two. 2^22 is 87 s at 48 kHz -- comfortably over a 16-bar lap
        with room to widen the window without reallocating. */
    static constexpr uint32_t ringLog2 = 22;

    void* rustCore = nullptr;
    std::atomic<double> sampleRate { 48000.0 };
    /** Guards the core against being destroyed while processBlock is inside it. */
    juce::CriticalSection coreLock;
    std::atomic<int> pending { 0 };

    JUCE_DECLARE_NON_COPYABLE_WITH_LEAK_DETECTOR (WaverollProcessor)
};
