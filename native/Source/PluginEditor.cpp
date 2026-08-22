#include "PluginEditor.h"

WaverollEditor::WaverollEditor (WaverollProcessor& p)
    : AudioProcessorEditor (&p), plugin (p)
{
    addAndMakeVisible (source);
    status.setJustificationType (juce::Justification::centred);
    status.setColour (juce::Label::textColourId, juce::Colour (0xff97a1b2));
    addAndMakeVisible (status);
    hint.setJustificationType (juce::Justification::centred);
    hint.setColour (juce::Label::textColourId, juce::Colour (0xff4a5262));
    hint.setFont (juce::FontOptions (11.0f));
    hint.setText ("1-0 select the last N x 10%   .  m mark  .  n from the mark  .  "
                  "h hold  .  d downbeat  .  esc clear",
                  juce::dontSendNotification);
    addAndMakeVisible (hint);
    // Without this the host swallows every keystroke and none of the above works. The plugin is
    // declared EDITOR_WANTS_KEYBOARD_FOCUS so a host will hand them over at all.
    setWantsKeyboardFocus (true);
    setSize (620, 260);
    startTimerHz (20);
}

WaverollEditor::~WaverollEditor()
{
    // Deliberately not deleted. A host references dropped audio by path until the set is
    // collected, so removing it here would break somebody's project days later with no trace.
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
    hint.setBounds (area.removeFromBottom (20));
    status.setBounds (area.removeFromBottom (44));
    source.setBounds (area.reduced (0, 10));
    if (plugin.core() != nullptr)
        wr_set_width (plugin.core(), (uint32_t) juce::jmax (1, getWidth()));
}

void WaverollEditor::timerCallback()
{
    if (plugin.core() == nullptr)
        return;
    WrStatus s {};
    wr_status (plugin.core(), &s);

    const auto rate = plugin.currentSampleRate();
    juce::String text = s.held ? "frozen" : s.playing ? "capturing" : "stopped";
    text << "   " << juce::String (s.bpm, 2) << " BPM"
         << "   " << juce::String (s.captured / juce::jmax (1.0, rate), 1) << " s"
         << "   bar " << juce::String (s.head * s.window_bars + 1.0, 2)
         << " of " << juce::String (s.window_bars, 0)
         << "   grid " << formatUnit (s.unit_bars);
    if (s.has_selection)
    {
        text << "      selection " << juce::String (s.selection_bars, 2) << " bars";
        if (s.selection_state == 1) text << " (not captured yet)";
        if (s.selection_state == 2) text << " (overwritten)";
    }
    status.setText (text, juce::dontSendNotification);
    source.setEnabled (s.selection_state == 3);
}

bool WaverollEditor::keyPressed (const juce::KeyPress& key)
{
    auto* core = plugin.core();
    if (core == nullptr)
        return false;

    const auto character = key.getTextCharacter();
    if (character >= '0' && character <= '9')
    {
        // Head to tail: the last N x 10% of the window, as whole cells.
        wr_select_percent (core, character == '0' ? 10 : (uint32_t) (character - '0'));
        return true;
    }
    switch (character)
    {
        case 'm': wr_mark (core); return true;
        case 'n': wr_select_from_marker (core); return true;
        case 'h': wr_hold (core, ! held); held = ! held; return true;
        case 'd': wr_set_downbeat_now (core); return true;
        default: break;
    }
    if (key == juce::KeyPress::escapeKey)
    {
        wr_clear_selection (core);
        return true;
    }
    return false;
}

juce::String WaverollEditor::formatUnit (double bars)
{
    if (bars >= 1.0)
        return juce::String (bars, 0);
    return "1/" + juce::String (juce::roundToInt (1.0 / juce::jmax (1.0e-9, bars)));
}

/**
 * Asks the core for the selection as a WAV and puts it on disk.
 *
 * The bytes come from Rust -- the same writer the browser build uses, with the same `acid` chunk
 * that makes a host warp the drop to its own tempo -- and the only thing C++ contributes is a
 * path. Returns a null file when the core refuses, which it does for a selection the writer has
 * lapped: a partly-overwritten read is a seam of old and new audio that looks entirely plausible
 * and is not what anybody selected.
 */
juce::File WaverollEditor::materialise()
{
    if (plugin.core() == nullptr)
        return {};

    // Samples since midnight, for the Broadcast Wave timestamp a host reads to spot a file back
    // to where it was captured.
    const auto now = juce::Time::getCurrentTime();
    const auto secondsToday = now.getHours() * 3600 + now.getMinutes() * 60 + now.getSeconds();
    const auto reference = (uint64_t) (secondsToday * plugin.currentSampleRate());

    const auto length = wr_stage (plugin.core(), reference);
    if (length == 0)
        return {};
    const auto* bytes = wr_staged_bytes (plugin.core());
    if (bytes == nullptr)
        return {};

    WrStatus s {};
    wr_status (plugin.core(), &s);
    const auto bars = juce::String (wr_selection_bars (plugin.core()), 2)
                          .trimCharactersAtEnd ("0").trimCharactersAtEnd (".").replace (".", "-");
    const auto stem = "waveroll_" + juce::String (juce::roundToInt (s.bpm)) + "bpm_" + bars + "bars";

    // A folder of its own rather than the system temp root, so everything this drops is in one
    // place a person can find and clear out. Never deleted afterwards: a host references dropped
    // audio by path until the set is collected, and removing the file breaks the project
    // silently, days later.
    auto directory = juce::File::getSpecialLocation (juce::File::userMusicDirectory)
                         .getChildFile ("Waveroll");
    directory.createDirectory();
    auto file = directory.getNonexistentChildFile (stem, ".wav", false);
    file.replaceWithData (bytes, length);
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
    const auto* label = ! isEnabled() ? "select something first"
                      : dragging      ? "dragging..."
                                      : "Drag this into the arrangement";
    g.setColour (juce::Colour (isEnabled() ? 0xfff0b342 : 0xff4a5262));
    g.drawText (label, getLocalBounds(), juce::Justification::centred);
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
    {
        dragging = false;
        repaint();
        return;
    }

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
