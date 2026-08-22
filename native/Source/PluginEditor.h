#pragma once
#include "PluginProcessor.h"

void*  waverollCreateNativeView (int width, int height);
void   waverollReleaseNativeView (void* handle);
double waverollViewScale (void* handle);

/**
 * The editor: a GPU surface with the rolling buffer on it, and the mouse rules that turn it into a
 * selection interface.
 */
class WaverollEditor : public juce::AudioProcessorEditor,
                       private juce::Timer
{
public:
    explicit WaverollEditor (WaverollProcessor&);
    ~WaverollEditor() override;

    void paint (juce::Graphics&) override;
    void resized() override;
    bool keyPressed (const juce::KeyPress&) override;

    /**
     * The picture, and the surface the mouse acts on.
     *
     * An NSViewComponent because the renderer needs a real native view to attach a Metal layer to;
     * the view is hit-transparent, so this component still receives every mouse event.
     */
    class Canvas : public juce::NSViewComponent
    {
    public:
        explicit Canvas (WaverollEditor& owner) : editor (owner) {}
        ~Canvas() override;

        void open();
        void close();
        void drawFrame();
        void resized() override;

        void mouseDown (const juce::MouseEvent&) override;
        void mouseDrag (const juce::MouseEvent&) override;
        void mouseUp (const juce::MouseEvent&) override;
        void mouseMove (const juce::MouseEvent&) override;
        void mouseWheelMove (const juce::MouseEvent&, const juce::MouseWheelDetails&) override;

        juce::String describe() const;
        bool ready() const noexcept { return gpuView != nullptr; }

    private:
        double fractionOf (const juce::MouseEvent&) const;
        bool overSelection (const juce::MouseEvent&) const;

        WaverollEditor& editor;
        void* native = nullptr;
        void* gpuView = nullptr;
        double dragFrom = 0.0;
        bool dragging = false;
        bool grabbing = false;
        bool moved = false;
    };

private:
    enum class Setting { Grid, Zoom, Window, Fit };
    void step (Setting, int direction);
    void timerCallback() override;
    juce::File materialise();
    juce::File materialiseMidi();
    static juce::String formatUnit (double bars);

    /**
     * A footer control: a glyph in a box, sized to sit beside a readout.
     *
     * Its own class rather than a TextButton because the default look is a rounded slab designed
     * to be pressed, and these need to be the smallest thing that can still be hit -- a plugin
     * footer is not where a person wants furniture.
     */
    class Tiny : public juce::Button
    {
    public:
        Tiny (const juce::String& glyph, const juce::String& tip, std::function<void()> action);
        void paintButton (juce::Graphics&, bool highlighted, bool down) override;
    private:
        juce::String glyph;
    };

    /** A readout with a pair of steppers beside it. */
    struct Stepper
    {
        juce::Label value;
        std::unique_ptr<Tiny> down;
        std::unique_ptr<Tiny> up;
        int layout (juce::Rectangle<int>& footer, int valueWidth);
    };

    WaverollProcessor& plugin;
    Canvas canvas { *this };
    juce::Label status;
    juce::Label hint;
    juce::Label gridLabel;
    juce::Label zoomLabel;
    Stepper grid;
    Stepper zoom;
    std::unique_ptr<Tiny> fit;
    juce::File staged;
    bool held = false;

    friend class Canvas;
    JUCE_DECLARE_NON_COPYABLE_WITH_LEAK_DETECTOR (WaverollEditor)
};
