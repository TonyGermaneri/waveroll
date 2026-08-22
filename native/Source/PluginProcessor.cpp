#include "PluginProcessor.h"
#include "PluginEditor.h"

WaverollProcessor::WaverollProcessor()
    : AudioProcessor (BusesProperties()
                          .withInput  ("Input",  juce::AudioChannelSet::stereo(), true)
                          .withOutput ("Output", juce::AudioChannelSet::stereo(), true))
{
}

void WaverollProcessor::prepareToPlay (double sampleRate, int)
{
    snapshot.sampleRate.store (sampleRate);
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

    const bool offline = isNonRealtime();
    snapshot.offline.store (offline);

    bool playing = false;
    if (auto* head = getPlayHead())
    {
        if (auto position = head->getPosition())
        {
            playing = position->getIsPlaying();
            if (auto bpm = position->getBpm())
                snapshot.bpm.store (*bpm);
        }
    }
    snapshot.playing.store (playing);

    // Capture follows the transport, and refuses an offline render outright. During a bounce the
    // host calls this far faster than realtime with the transport reporting "playing" the whole
    // way, so without this guard exporting a track quietly replaces the take with the export.
    if (playing && ! offline)
        snapshot.captured.fetch_add (buffer.getNumSamples());

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
