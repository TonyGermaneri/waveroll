// A bare NSView for wgpu to attach a Metal layer to.
//
// JUCE has no Metal integration, so the renderer is given a view of its own and JUCE hosts it with
// NSViewComponent. That keeps the two entirely separate: JUCE never paints over the surface, and
// the renderer never has to know what a juce::Component is.

#include <juce_gui_basics/juce_gui_basics.h>

#import <Cocoa/Cocoa.h>

/**
 * Hit-transparent, and that is the whole design.
 *
 * A native child view sits above everything JUCE draws and would otherwise swallow every mouse
 * event in the picture -- which is the entire interactive surface. Returning nil from hitTest:
 * lets the events fall through to JUCE's own view, so Metal draws on top and JUCE still gets the
 * mouse. The alternative, forwarding events back out of Objective-C into the component, means
 * reimplementing hit testing, capture and modifier handling that JUCE already does.
 */
@interface WaverollMetalView : NSView
@end

@implementation WaverollMetalView
- (NSView*) hitTest: (NSPoint) point
{
    (void) point;
    return nil;
}
@end

void* waverollCreateNativeView (int width, int height)
{
    NSView* view = [[WaverollMetalView alloc] initWithFrame: NSMakeRect (0, 0, width, height)];
    // Layer-backed, and the layer resizes with the view. Without this wgpu gets a zero-sized
    // drawable and every frame is silently dropped.
    view.wantsLayer = YES;
    view.layerContentsRedrawPolicy = NSViewLayerContentsRedrawDuringViewResize;
    view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    return (void*) view;
}

void waverollReleaseNativeView (void* handle)
{
    if (handle != nullptr)
        [(NSView*) handle release];
}

/// Backing scale of the screen the view is on, so the renderer draws at the display's real
/// resolution rather than a blurry upscale of the logical size.
double waverollViewScale (void* handle)
{
    if (handle == nullptr)
        return 1.0;
    NSView* view = (NSView*) handle;
    NSWindow* window = [view window];
    return window != nil ? [window backingScaleFactor] : 2.0;
}
