#include "PluginEditor.h"

WaverollEditor::WaverollEditor (WaverollProcessor& p)
    : AudioProcessorEditor (&p), plugin (p)
{
    addAndMakeVisible (source);
    status.setJustificationType (juce::Justification::centred);
    status.setColour (juce::Label::textColourId, juce::Colour (0xff97a1b2));
    addAndMakeVisible (status);
    setSize (620, 260);
    startTimerHz (20);
}

WaverollEditor::~WaverollEditor()
{
    if (staged.existsAsFile())
        staged.deleteFile();
}

void WaverollEditor::paint (juce::Graphics& g)
{
    g.fillAll (juce::Colour (0xff0b0e13));
    g.setColour (juce::Colour (0xff6b7484));
    g.setFont (juce::FontOptions (12.0f));
    g.drawText ("WAVEROLL", getLocalBounds().removeFromTop (34), juce::Justification::centred);
}

void WaverollEditor::resized()
{
    auto area = getLocalBounds().reduced (24);
    area.removeFromTop (20);
    status.setBounds (area.removeFromBottom (56));
    source.setBounds (area.reduced (0, 10));
}

void WaverollEditor::timerCallback()
{
    const auto captured = plugin.snapshot.captured.load();
    const auto rate = plugin.snapshot.sampleRate.load();
    status.setText (juce::String (plugin.snapshot.playing.load() ? "capturing" : "stopped")
                        + (plugin.snapshot.offline.load() ? "  (offline render - not captured)" : "")
                        + "   " + juce::String (plugin.snapshot.bpm.load(), 2) + " BPM"
                        + "   " + juce::String (captured / juce::jmax (1.0, rate), 1) + " s captured",
                    juce::dontSendNotification);
}

/**
 * Writes four bars of a tone to a real file on disk.
 *
 * A placeholder for the ring, deliberately: what is being measured first is whether the *drag*
 * is accepted, and a synthetic file answers that as well as a captured one while depending on
 * nothing.
 */
juce::File WaverollEditor::materialise()
{
    const auto rate = plugin.snapshot.sampleRate.load();
    const auto bpm = plugin.snapshot.bpm.load();
    const int frames = (int) std::round (rate * (240.0 / juce::jmax (20.0, bpm)));  // four bars of 4/4

    juce::AudioBuffer<float> buffer (2, frames);
    for (int i = 0; i < frames; ++i)
    {
        const auto t = (double) i / rate;
        const auto beat = 60.0 / bpm;
        const auto phase = std::fmod (t / beat, 1.0);
        const float kick = (float) (std::sin (juce::MathConstants<double>::twoPi * 90.0 * phase * beat)
                                    * std::exp (-phase * 20.0) * 0.8);
        const float bass = (float) (std::sin (juce::MathConstants<double>::twoPi * 55.0 * t) * 0.3);
        buffer.setSample (0, i, kick + bass);
        buffer.setSample (1, i, kick + bass);
    }

    auto file = juce::File::getSpecialLocation (juce::File::tempDirectory)
                    .getChildFile ("waveroll_" + juce::String ((int) std::round (bpm)) + "bpm_4bars.wav");
    file.deleteFile();
    juce::WavAudioFormat format;
    if (auto stream = std::unique_ptr<juce::FileOutputStream> (file.createOutputStream()))
    {
        if (auto writer = std::unique_ptr<juce::AudioFormatWriter> (
                format.createWriterFor (stream.get(), rate, 2, 24, {}, 0)))
        {
            stream.release();  // the writer owns it now
            writer->writeFromAudioSampleBuffer (buffer, 0, frames);
        }
    }
    return file;
}

void WaverollEditor::DragSource::paint (juce::Graphics& g)
{
    const auto bounds = getLocalBounds().toFloat().reduced (1.0f);
    g.setColour (juce::Colour (hot ? 0xff2a2313 : 0xff141922));
    g.fillRoundedRectangle (bounds, 5.0f);
    g.setColour (juce::Colour (0xfff0b342));
    g.drawRoundedRectangle (bounds, 5.0f, hot ? 2.0f : 1.0f);
    g.setFont (juce::FontOptions (14.0f));
    g.drawText (dragging ? "dragging..." : "Drag this into the arrangement",
                getLocalBounds(), juce::Justification::centred);
}

void WaverollEditor::DragSource::mouseEnter (const juce::MouseEvent&) { hot = true; repaint(); }
void WaverollEditor::DragSource::mouseExit  (const juce::MouseEvent&) { hot = false; dragging = false; repaint(); }

void WaverollEditor::DragSource::mouseDrag (const juce::MouseEvent& event)
{
    if (dragging || event.getDistanceFromDragStart() < 6)
        return;
    dragging = true;
    repaint();

    editor.staged = editor.materialise();
    if (! editor.staged.existsAsFile())
        return;

    // Started synchronously from inside the mouse handler, and this is the fiddly part: on macOS a
    // drag session has to begin while the originating event is still current. Queuing it -- onto
    // the message loop, or behind an async file write -- produces a drag that never starts, with
    // no error anywhere.
    juce::StringArray files;
    files.add (editor.staged.getFullPathName());
    juce::DragAndDropContainer::performExternalDragDropOfFiles (
        files, /* canMoveFiles */ false, this,
        [this] { dragging = false; repaint(); });
}
