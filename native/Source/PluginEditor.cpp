#include "PluginEditor.h"

namespace waveroll { const char* buildStamp(); }

WaverollEditor::WaverollEditor (WaverollProcessor& p)
    : AudioProcessorEditor (&p), plugin (p)
{
    addAndMakeVisible (canvas);
    status.setJustificationType (juce::Justification::centredLeft);
    status.setColour (juce::Label::textColourId, juce::Colour (0xff97a1b2));
    status.setFont (juce::FontOptions (12.0f));
    addAndMakeVisible (status);

    hint.setJustificationType (juce::Justification::centredRight);
    hint.setColour (juce::Label::textColourId, juce::Colour (0xff4a5262));
    hint.setFont (juce::FontOptions (11.0f));
    hint.setText ("drag inside the selection to take it  .  1-0 last N x 10%  .  m mark  .  "
                  "n from mark  .  h hold  .  d downbeat",
                  juce::dontSendNotification);
    addAndMakeVisible (hint);

    // Without this the host swallows every keystroke. EDITOR_WANTS_KEYBOARD_FOCUS makes a host
    // willing to hand them over at all; this makes the editor willing to take them.
    setWantsKeyboardFocus (true);
    setResizable (true, true);
    setResizeLimits (520, 220, 4000, 1600);
    setSize (900, 340);
    startTimerHz (60);
    canvas.open();
}

WaverollEditor::~WaverollEditor()
{
    stopTimer();
    canvas.close();
    // The staged file is deliberately left behind: a host references dropped audio by path until
    // the set is collected, so deleting it here would break somebody's project days later.
}

void WaverollEditor::paint (juce::Graphics& g)
{
    g.fillAll (juce::Colour (0xff0b0e13));
    if (! canvas.ready())
    {
        g.setColour (juce::Colour (0xffff8d7d));
        g.setFont (juce::FontOptions (13.0f));
        g.drawText ("The GPU surface could not be opened. Capture and drag still work.",
                    canvas.getBounds(), juce::Justification::centred);
    }
}

void WaverollEditor::resized()
{
    auto area = getLocalBounds();
    auto footer = area.removeFromBottom (22);
    status.setBounds (footer.removeFromLeft (footer.getWidth() * 2 / 3).reduced (10, 0));
    hint.setBounds (footer.reduced (10, 0));
    canvas.setBounds (area);
}

void WaverollEditor::timerCallback()
{
    if (plugin.core() == nullptr)
        return;

    canvas.drawFrame();

    WrStatus s {};
    wr_status (plugin.core(), &s);
    const auto rate = plugin.currentSampleRate();
    juce::String text = s.held ? "FROZEN" : s.playing ? "capturing" : "stopped";
    text << "   " << juce::String (s.bpm, 2) << " BPM"
         << "   bar " << juce::String (s.head * s.window_bars + 1.0, 2)
         << " of " << juce::String (s.window_bars, 0)
         << "   lap " << juce::String ((int) s.lap)
         << "   grid " << formatUnit (s.unit_bars)
         << "   " << juce::String (s.captured / juce::jmax (1.0, rate), 1) << " s";
    if (s.markers > 0)
        text << "   marks " << juce::String ((int) s.markers);
    if (s.has_selection)
    {
        text << "      sel " << juce::String (s.selection_bars, 2) << " bars";
        if (s.selection_state == 1) text << " (not captured yet)";
        else if (s.selection_state == 2) text << " (overwritten)";
    }
    status.setText (text, juce::dontSendNotification);
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


/**
 * The same selection as a Standard MIDI File, or nothing when the lane is empty over it.
 *
 * Nothing is the right answer rather than an empty clip: a host handed a MIDI file with no notes
 * creates a track for it, and a silent track nobody asked for is worse than one fewer file.
 */
juce::File WaverollEditor::materialiseMidi()
{
    auto* core = plugin.core();
    if (core == nullptr)
        return {};
    const auto length = wr_stage_midi (core, /* let_ring */ false);
    if (length == 0)
        return {};
    const auto* bytes = wr_staged_midi_bytes (core);
    if (bytes == nullptr)
        return {};

    // Named after the audio it came from, so the pair is obvious in a folder and in a set.
    auto file = staged.getSiblingFile (staged.getFileNameWithoutExtension() + ".mid");
    file.replaceWithData (bytes, length);
    return file;
}

// ---------------------------------------------------------------------------------------
// The canvas
// ---------------------------------------------------------------------------------------

WaverollEditor::Canvas::~Canvas() { close(); }

void WaverollEditor::Canvas::open()
{
    if (native != nullptr)
        return;
    native = waverollCreateNativeView (juce::jmax (1, getWidth()), juce::jmax (1, getHeight()));
    setView (native);
    if (auto* core = editor.plugin.core())
    {
        const auto scale = waverollViewScale (native);
        gpuView = wr_view_open (core, native,
                                (uint32_t) juce::jmax (1, juce::roundToInt (getWidth() * scale)),
                                (uint32_t) juce::jmax (1, juce::roundToInt (getHeight() * scale)),
                                scale);
    }
}

void WaverollEditor::Canvas::close()
{
    // Order matters: the surface holds the view, so it goes first.
    wr_view_close (gpuView);
    gpuView = nullptr;
    setView (nullptr);
    waverollReleaseNativeView (native);
    native = nullptr;
}

void WaverollEditor::Canvas::resized()
{
    NSViewComponent::resized();
    if (gpuView == nullptr)
        return;
    const auto scale = waverollViewScale (native);
    wr_view_resize (gpuView,
                    (uint32_t) juce::jmax (1, juce::roundToInt (getWidth() * scale)),
                    (uint32_t) juce::jmax (1, juce::roundToInt (getHeight() * scale)),
                    scale);
}

void WaverollEditor::Canvas::drawFrame()
{
    if (gpuView != nullptr)
        wr_view_draw (editor.plugin.core(), gpuView);
}

juce::String WaverollEditor::Canvas::describe() const
{
    char buffer[128] {};
    wr_view_describe (gpuView, (uint8_t*) buffer, sizeof (buffer));
    return juce::String (buffer);
}

double WaverollEditor::Canvas::fractionOf (const juce::MouseEvent& event) const
{
    return juce::jlimit (0.0, 1.0, (double) event.x / juce::jmax (1.0, (double) getWidth()));
}

/**
 * Whether the pointer is over the selection.
 *
 * The rule that separates grabbing from selecting, and the same one every arrangement view uses: a
 * drag that starts inside a selection carries it, a drag anywhere else starts a new one. Alt forces
 * a new selection, for when the thing you want is underneath the thing you already have.
 */
bool WaverollEditor::Canvas::overSelection (const juce::MouseEvent& event) const
{
    auto* core = editor.plugin.core();
    if (core == nullptr || event.mods.isAltDown())
        return false;
    WrStatus s {};
    wr_status (core, &s);
    if (! s.has_selection || s.selection_state != 3)
        return false;
    const auto at = fractionOf (event);
    return at >= juce::jmin (s.selection_from, s.selection_to)
        && at <= juce::jmax (s.selection_from, s.selection_to);
}

void WaverollEditor::Canvas::mouseMove (const juce::MouseEvent& event)
{
    setMouseCursor (overSelection (event) ? juce::MouseCursor::DraggingHandCursor
                                          : juce::MouseCursor::CrosshairCursor);
}

void WaverollEditor::Canvas::mouseDown (const juce::MouseEvent& event)
{
    dragFrom = fractionOf (event);
    grabbing = overSelection (event);
    dragging = false;
    moved = false;
    editor.grabKeyboardFocus();
}

void WaverollEditor::Canvas::mouseDrag (const juce::MouseEvent& event)
{
    auto* core = editor.plugin.core();
    if (core == nullptr)
        return;

    // A pointer that has not travelled is a click, not a drag. Without this every click becomes a
    // one-pixel drag, and at high zoom those select different things.
    if (! moved && event.getDistanceFromDragStart() < 3)
        return;
    moved = true;

    if (! grabbing)
    {
        wr_drag (core, dragFrom, fractionOf (event));
        return;
    }
    if (dragging)
        return;
    dragging = true;

    editor.staged = editor.materialise();
    if (! editor.staged.existsAsFile())
    {
        dragging = false;
        return;
    }

    // Started synchronously from inside the mouse handler, and this is the fiddly part: on macOS a
    // drag session must begin while the originating event is still current. Queue it -- onto the
    // message loop, or behind an async write -- and the drag never starts, with no error anywhere.
    juce::StringArray files;
    files.add (editor.staged.getFullPathName());
    // If anything was played over this selection, the MIDI goes with it. A host turns two dropped
    // files into two tracks, which is exactly right: the take, and what played it.
    if (auto notes = editor.materialiseMidi(); notes.existsAsFile())
        files.add (notes.getFullPathName());

    juce::DragAndDropContainer::performExternalDragDropOfFiles (
        files, false, this, [this] { dragging = false; });
}

void WaverollEditor::Canvas::mouseUp (const juce::MouseEvent& event)
{
    if (auto* core = editor.plugin.core(); core != nullptr && ! moved && ! grabbing)
        wr_click (core, fractionOf (event));
    grabbing = false;
    moved = false;
}

juce::AudioProcessorEditor* createWaverollEditor (WaverollProcessor& p)
{
    return new WaverollEditor (p);
}
