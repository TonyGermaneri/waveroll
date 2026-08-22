#pragma once
#include "PluginProcessor.h"

/**
 * The editor, and for now the one measurement that decides the project's shape: whether a drag out
 * of a *plugin* editor is accepted by a host's own arrangement.
 *
 * Dragging out of a plugin is strictly harder than out of an app, because the editor lives inside
 * the host's view hierarchy, so proving it here proves it everywhere.
 */
class WaverollEditor : public juce::AudioProcessorEditor,
                       private juce::Timer
{
public:
    explicit WaverollEditor (WaverollProcessor&);
    ~WaverollEditor() override;

    void paint (juce::Graphics&) override;
    void resized() override;

    /** The region you drag from. Kept as its own component so the drag begins from a hit test
        rather than from anywhere in the window. */
    class DragSource : public juce::Component
    {
    public:
        explicit DragSource (WaverollEditor& owner) : editor (owner) {}
        void paint (juce::Graphics&) override;
        void mouseDrag (const juce::MouseEvent&) override;
        void mouseEnter (const juce::MouseEvent&) override;
        void mouseExit (const juce::MouseEvent&) override;

    private:
        WaverollEditor& editor;
        bool hot = false;
        bool dragging = false;
    };

private:
    void timerCallback() override;
    /** Writes the file that will be dragged, returning its path. */
    juce::File materialise();

    WaverollProcessor& processor;
    DragSource source { *this };
    juce::Label status;
    juce::File staged;
    juce::TemporaryFile* keepAlive = nullptr;

    friend class DragSource;
    JUCE_DECLARE_NON_COPYABLE_WITH_LEAK_DETECTOR (WaverollEditor)
};
