/**
 * * Creating a sidebar enables you to:
 - create an ordered group of docs
 - render a sidebar for each doc of that group
 - provide next/previous navigation

 The sidebars can be generated from the filesystem, or explicitly defined here.

 Create as many sidebars as you want.
 */

module.exports = {
    docs: [{
        type: 'doc',
        id: 'welcome',
    },
        {
            type: 'category',
            label: 'Getting Started',
            items: [
                {
                    type: 'doc',
                    id: 'getting_started/overview',
                    label: 'Overview',
                },
                {
                    type: 'doc',
                    id: 'getting_started/rust_getting_started',
                    label: 'Rust',
                },
                {
                    type: 'doc',
                    id: 'getting_started/wasm_getting_started',
                    label: 'Wasm Binding',
                },
                {
                    type: 'doc',
                    id: 'getting_started/c_getting_started',
                    label: 'C Binding',
                },
            ]
        },
        {
            type: 'category',
            label: 'How Tos',
            items:
                [
                    {
                        type: 'doc',
                        id: 'how_tos/rust_how_tos',
                        label: 'Rust',
                    },
                    {
                        type: 'doc',
                        id: 'how_tos/c_how_tos',
                        label: 'C Binding',
                    },
                    {
                        type: 'doc',
                        id: 'how_tos/wasm_how_tos',
                        label: 'Wasm Binding',
                    }
                ]
        },
        {
            type: 'category',
            label: 'Key Concepts',
            items:
                [
                    {
                        type: 'category',
                        label: 'Channels Protocol',
                        items: [
                                'key_concepts/channels_protocol/overview',
                            'key_concepts/channels_protocol/authors',
                            'key_concepts/channels_protocol/subscribers',
                            'key_concepts/channels_protocol/branching',
                            'key_concepts/channels_protocol/keyloads',
                            'key_concepts/channels_protocol/sequencing',
                        ],
                    }
                ]
        },
        {
            type: 'category',
            label: 'Reference',
            items: [
                'reference/rust_api_reference',
                'reference/c_api_reference',
                'reference/wasm_api_reference',
                'reference/specs',
            ]
        },
        {
            type: 'doc',
            id: 'troubleshooting',
            label: 'Troubleshooting'
        },
        {
            type: 'doc',
            id: 'contribute',
            label: 'Contribute',
        }
    ]
};
