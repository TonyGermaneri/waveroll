#include "PluginProcessor.h"
#include "PluginEditor.h"

WaverollProcessor::WaverollProcessor()
    : AudioProcessor (BusesProperties()
                          .withInput  ("Input",  juce::AudioChannelSet::stereo(), true)
                          .withOutput ("Output", juce::AudioChannelSet::stereo(), true))
{
}

WaverollProcessor::~WaverollProcessor()
{
    const juce::ScopedLock lock (coreLock);
    wr_destroy (rustCore);
    rustCore = nullptr;
}

void WaverollProcessor::prepareToPlay (double rate, int samplesPerBlock)
{
    sampleRate.store (rate);
    const juce::ScopedLock lock (coreLock);
    // Re-created rather than reconfigured: the ring's capacity and the tempo map's sample rate are
    // both fixed at construction, and a rate change means the audio in the old buffer no longer
    // means what it did.
    wr_destroy (rustCore);
    rustCore = wr_create ((uint32_t) rate,
                          (uint32_t) juce::jlimit (1, 2, getTotalNumInputChannels()),
                          ringLog2,
                          (uint32_t) juce::jmax (64, samplesPerBlock));

    // Never call setLatencySamples: zero is the right answer and declaring anything makes the host
    // apply delay compensation and shift the track under the user.
}

bool WaverollProcessor::isBusesLayoutSupported (const BusesLayout& layouts) const
{
    // Whatever it is handed, in equals out. A tap has no opinion about channel counts.
    return layouts.getMainInputChannelSet() == layouts.getMainOutputChannelSet()
        && ! layouts.getMainInputChannelSet().isDisabled();
}

void WaverollProcessor::processBlock (juce::AudioBuffer<float>& buffer, juce::MidiBuffer&)
{
    juce::ScopedNoDenormals noDenormals;

    WrTransport transport {};
    transport.offline = isNonRealtime();
    transport.bpm = 120.0;
    transport.numerator = 4;
    transport.denominator = 4;

    if (auto* head = getPlayHead())
    {
        if (auto position = head->getPosition())
        {
            transport.playing = position->getIsPlaying();
            if (auto bpm = position->getBpm())
                transport.bpm = *bpm;
            if (auto signature = position->getTimeSignature())
            {
                transport.numerator = (uint32_t) signature->numerator;
                transport.denominator = (uint32_t) signature->denominator;
            }
        }
    }

    // tryEnter rather than enter: the audio thread must never wait on the message thread, and a
    // block dropped while the core is being rebuilt is a block of audio, not a glitch.
    const juce::ScopedTryLock lock (coreLock);
    if (lock.isLocked() && rustCore != nullptr)
    {
        const int channels = juce::jlimit (1, 2, buffer.getNumChannels());
        const float* pointers[2] { nullptr, nullptr };
        for (int c = 0; c < channels; ++c)
            pointers[c] = buffer.getReadPointer (c);

        // Capture follows the transport and refuses an offline render, which is enforced inside
        // the core. During a bounce the host calls this far faster than realtime with the
        // transport reporting "playing" the whole way, so without that guard exporting a track
        // quietly replaces the take with the export.
        wr_capture (rustCore, pointers, (uint32_t) buffer.getNumSamples(), &transport);
    }

    // The buffer is deliberately untouched. In equals out, sample for sample.
    juce::ignoreUnused (buffer);
}

void WaverollProcessor::getStateInformation (juce::MemoryBlock& destination)
{
    juce::ValueTree state ("WAVEROLL");
    state.setProperty ("version", 1, nullptr);
    juce::MemoryOutputStream stream (destination, false);
    state.writeToStream (stream);
}

void WaverollProcessor::setStateInformation (const void* data, int size)
{
    juce::ignoreUnused (data, size);
}

juce::AudioProcessorEditor* WaverollProcessor::createEditor()
{
    return new WaverollEditor (*this);
}

juce::AudioProcessor* JUCE_CALLTYPE createPluginFilter()
{
    return new WaverollProcessor();
}
